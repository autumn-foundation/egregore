//! End-to-end tests for `eg query tests <handle>` (issue #126): maps a
//! symbol to the test symbols that can reach it over inbound `CALLS` edges.
//!
//! The CLI face, the NDJSON contract, the exit-code families, the temporal
//! selectors, determinism, the daemon `tests_for_symbol` verb, and the
//! precision probe (AC7: comments / string literals that merely mention the
//! symbol must not produce covering-test rows).
#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, SymbolRole, TemporalMetadata,
    ir::{Graph, SCHEMA_VERSION, stable_id},
};
use assert_cmd::Command;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn symbol(graph: &mut Graph, path: &str, name: &str, lines: (usize, usize), test: bool) -> String {
    let id = sym_id(path, name);
    let rec = GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(lines.0, lines.1),
        name.to_owned(),
        "rust",
        format!("fn {name} in {path}"),
    );
    let rec = if test {
        rec.with_role(SymbolRole::Test)
    } else {
        rec
    };
    graph.push(rec);
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id(path),
        id.clone(),
        None,
        format!("{path} defines {name}"),
    ));
    id
}

fn file(graph: &mut Graph, repo_id: &str, path: &str) {
    let fid = file_id(path);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 200),
        path.rsplit('/').next().unwrap().to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        fid,
        None,
        format!("repo contains {path}"),
    ));
}

fn calls(graph: &mut Graph, from: &str, to: &str) {
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Calls,
            from.to_owned(),
            to.to_owned(),
            Some("1.0".to_owned()),
            "call edge".to_owned(),
        )
        .with_resolution(CallResolution::Resolved),
    );
}

fn references(graph: &mut Graph, from: &str, to: &str) {
    graph.push(GraphRecord::edge(
        EdgeLabel::References,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        "reference edge".to_owned(),
    ));
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
}

#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("covering_tests.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-ct"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-ct".to_owned()),
        "Repository repo-ct".to_owned(),
    ));

    for p in [
        "src/app.rs",
        "src/app_tests.rs",
        "src/helpers.rs",
        "src/e2e.rs",
        "src/other.rs",
        "src/pad.rs",
        "src/cycle.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // Target and its inbound neighborhood.
    let target_id = symbol(&mut graph, "src/app.rs", "process_order", (5, 20), false);
    // Direct test caller.
    let direct_test = symbol(
        &mut graph,
        "src/app_tests.rs",
        "test_process_order",
        (5, 12),
        true,
    );
    calls(&mut graph, &direct_test, &target_id);
    // Non-test direct caller: traversed, never reported.
    let prod_caller = symbol(
        &mut graph,
        "src/helpers.rs",
        "audit_logging",
        (5, 12),
        false,
    );
    calls(&mut graph, &prod_caller, &target_id);
    // Transitive test caller across files: test_end_to_end -> validate -> process_order.
    let helper = symbol(&mut graph, "src/helpers.rs", "validate", (14, 22), false);
    calls(&mut graph, &helper, &target_id);
    let e2e_test = symbol(&mut graph, "src/e2e.rs", "test_end_to_end", (5, 12), true);
    calls(&mut graph, &e2e_test, &helper);
    // Test that only references the target: excluded from rows.
    let ref_test = symbol(
        &mut graph,
        "src/app_tests.rs",
        "test_mentions_only",
        (14, 22),
        true,
    );
    references(&mut graph, &ref_test, &target_id);
    // Symbol with no covering test at all.
    symbol(&mut graph, "src/other.rs", "orphan_fn", (5, 12), false);
    // Second symbol named `validate` in another file: makes the bare name
    // ambiguous for the exit-1 test.
    symbol(&mut graph, "src/pad.rs", "validate", (5, 12), false);
    // Mutual-recursion cycle behind a test: the walk must terminate.
    let cyc_a = symbol(&mut graph, "src/cycle.rs", "recur_a", (5, 12), false);
    let cyc_b = symbol(&mut graph, "src/cycle.rs", "recur_b", (14, 22), false);
    calls(&mut graph, &cyc_a, &cyc_b);
    calls(&mut graph, &cyc_b, &cyc_a);
    let cyc_test = symbol(&mut graph, "src/cycle.rs", "test_cycle", (24, 32), true);
    calls(&mut graph, &cyc_test, &cyc_a);
    // Tombstoned test: its rows must not appear.
    let tomb_test = symbol(
        &mut graph,
        "src/app_tests.rs",
        "test_removed",
        (24, 32),
        true,
    );
    calls(&mut graph, &tomb_test, &target_id);
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &tomb_test]),
        schema_version: SCHEMA_VERSION,
        deleted_id: tomb_test.clone(),
        summary: "test_removed was deleted".to_owned(),
        producer: None,
    });

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    Fixture {
        _temp: temp,
        graph: path,
    }
}

fn query_tests(graph: &PathBuf, handle: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
    egregore()
        .arg("query")
        .arg("tests")
        .arg(handle)
        .arg("--graph")
        .arg(graph)
        .args(extra)
        .assert()
}

fn ndjson_lines(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// AC8 — the four required integration cases
// ---------------------------------------------------------------------------

/// Direct test caller + transitive test caller across files, in one run.
#[test]
fn direct_and_transitive_test_callers_are_reported() {
    let fx = seed();
    let out = query_tests(&fx.graph, "process_order", &[]).success();

    let lines = ndjson_lines(&out.get_output().stdout);
    assert!(!lines.is_empty(), "header line must be present");
    let header = &lines[0];
    assert_eq!(header["ok"], true);
    assert_eq!(header["handle"], "process_order");
    assert_eq!(header["direction"], "inbound");
    assert_eq!(header["edge_labels"], Value::from(vec!["CALLS"]));
    assert_eq!(header["total_covering_tests"], 2);
    assert_eq!(header["direct_tests"], 1);
    assert_eq!(header["transitive_tests"], 1);
    assert_eq!(header["target"]["name"], "process_order");
    assert!(header["truncation"].is_null());
    assert!(
        header["disclaimer"]
            .as_str()
            .expect("disclaimer is a string")
            .contains("reachability LEADS")
    );

    let rows: Vec<&Value> = lines[1..].iter().collect();
    assert_eq!(rows.len(), 2, "exactly the two test rows");
    // Rows are ordered by (hop, record_id): the direct test comes first.
    assert_eq!(rows[0]["coverage"], "direct");
    assert_eq!(rows[0]["hop"], 1);
    assert_eq!(rows[0]["name"], "test_process_order");
    assert_eq!(rows[0]["trust"], "reachability_lead");
    assert_eq!(rows[1]["coverage"], "transitive");
    assert_eq!(rows[1]["hop"], 2);
    assert_eq!(rows[1]["name"], "test_end_to_end");
    // The transitive row carries its full call path: test -> validate -> target.
    let path = rows[1]["path"].as_array().expect("path is an array");
    assert_eq!(path.len(), 2);
    assert_eq!(path[0]["source_record_id"], rows[1]["record_id"]);
    assert_eq!(path[1]["target_record_id"], header["target"]["record_id"]);
}

/// A non-test caller is traversed but never reported; a reference-only test
/// is excluded; a tombstoned test is excluded.
#[test]
fn non_test_caller_reference_only_and_tombstoned_tests_are_excluded() {
    let fx = seed();
    let out = query_tests(&fx.graph, "process_order", &[]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    let names: Vec<&str> = lines[1..]
        .iter()
        .map(|r| r["name"].as_str().expect("name is a string"))
        .collect();
    assert!(
        !names.contains(&"audit_logging"),
        "production caller not reported"
    );
    assert!(
        !names.contains(&"test_mentions_only"),
        "reference-only test not reported"
    );
    assert!(
        !names.contains(&"test_removed"),
        "tombstoned test not reported"
    );
    assert!(
        !names.contains(&"validate"),
        "intermediate helper not reported"
    );
}

/// A symbol with no covering test returns exit 0 with an explicit empty row set.
#[test]
fn symbol_with_no_covering_test_is_an_empty_success() {
    let fx = seed();
    let out = query_tests(&fx.graph, "orphan_fn", &[]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines.len(), 1, "header line only");
    assert_eq!(lines[0]["ok"], true);
    assert_eq!(lines[0]["total_covering_tests"], 0);
    assert_eq!(lines[0]["direct_tests"], 0);
    assert_eq!(lines[0]["transitive_tests"], 0);
}

/// The mutual-recursion cycle terminates and the cycle's test still surfaces.
#[test]
fn cycle_terminates_and_cycle_test_surfaces() {
    let fx = seed();
    let out = query_tests(&fx.graph, "recur_a", &[]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1);
    assert_eq!(lines[1]["name"], "test_cycle");
    assert_eq!(lines[1]["coverage"], "direct");
}

// ---------------------------------------------------------------------------
// Exit codes and handle handling
// ---------------------------------------------------------------------------

#[test]
fn unknown_handle_is_exit_2_no_match() {
    let fx = seed();
    let out = query_tests(&fx.graph, "no_such_symbol", &[]).code(2);
    let payload: Value = serde_json::from_slice(&out.get_output().stdout).expect("stdout is JSON");
    assert_eq!(payload["error"]["code"], "no_match");
}

#[test]
fn ambiguous_name_is_exit_1() {
    let fx = seed();
    let out = query_tests(&fx.graph, "validate", &[]).code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(
        stderr.contains("ambiguous_handle"),
        "stderr carries the typed diagnostic, got: {stderr}"
    );
}

#[test]
fn zero_max_depth_is_exit_1() {
    let fx = seed();
    query_tests(&fx.graph, "process_order", &["--max-depth", "0"]).code(1);
}

#[test]
fn max_depth_bound_truncates_with_counts() {
    let fx = seed();
    // Depth 1 keeps only the direct test; the transitive path is dropped and counted.
    let out = query_tests(&fx.graph, "process_order", &["--max-depth", "1"]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1);
    assert_eq!(lines[1]["name"], "test_process_order");
    let truncation = &lines[0]["truncation"];
    assert_eq!(truncation["code"], "max_depth_truncated");
    assert_eq!(truncation["max_depth"], 1);
    assert!(truncation["dropped_total"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn text_format_renders_the_covering_tests() {
    let fx = seed();
    let out = query_tests(&fx.graph, "process_order", &["--format", "text"]).success();
    let text = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(
        text.contains("covering tests of process_order"),
        "got: {text}"
    );
    assert!(text.contains("test_process_order"));
    assert!(text.contains("test_end_to_end"));
    assert!(!text.contains("audit_logging"));
}

#[test]
fn two_runs_are_byte_identical() {
    let fx = seed();
    let a = query_tests(&fx.graph, "process_order", &[])
        .success()
        .get_output()
        .stdout
        .clone();
    let b = query_tests(&fx.graph, "process_order", &[])
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(a, b, "covering-tests output must be deterministic");
}

// ---------------------------------------------------------------------------
// Temporal selectors --at / --as-of
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";

fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// History fixture: at c1 `test_old` calls `anchor_h`; at c2 that call is
/// gone and `test_new` calls `anchor_h` instead.
fn seed_history() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("covering_tests_history.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-h"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-h".to_owned()),
        "Repository repo-h".to_owned(),
    ));
    let fid = file_id("src/h.rs");
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        "src/h.rs".to_owned(),
        span(1, 200),
        "h.rs".to_owned(),
        "rust",
        "Source file src/h.rs".to_owned(),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id,
        fid.clone(),
        None,
        "repo contains src/h.rs".to_owned(),
    ));

    let commit_node = |sha: &str, parents: &[&str], vt: &str| {
        GraphRecord::node(
            stable_id(&["node", "commit", "repo-h", sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(temporal(sha, parents, vt))
    };
    graph.push(commit_node("aaaa1111", &[], T1));
    graph.push(commit_node("bbbb2222", &["aaaa1111"], T2));

    let mut hist_symbol = |name: &str, commit: &str, vt: &str, test: bool| {
        let id = sym_id("src/h.rs", name);
        let rec = GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            "src/h.rs".to_owned(),
            span(1, 10),
            name.to_owned(),
            "rust",
            format!("fn {name}"),
        )
        .with_temporal(temporal(commit, &[], vt));
        let rec = if test {
            rec.with_role(SymbolRole::Test)
        } else {
            rec
        };
        graph.push(rec);
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Defines,
                fid.clone(),
                id.clone(),
                None,
                format!("src/h.rs defines {name}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        id
    };

    let anchor_id = hist_symbol("anchor_h", "aaaa1111", T1, false);
    hist_symbol("anchor_h", "bbbb2222", T2, false);
    let test_old = hist_symbol("test_old", "aaaa1111", T1, true);
    let test_new = hist_symbol("test_new", "bbbb2222", T2, true);

    let mut hist_call = |from: &str, to: &str, commit: &str, vt: &str| {
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                from.to_owned(),
                to.to_owned(),
                Some("1.0".to_owned()),
                "historical call".to_owned(),
            )
            .with_resolution(CallResolution::Resolved)
            .with_temporal(temporal(commit, &[], vt)),
        );
    };
    hist_call(&test_old, &anchor_id, "aaaa1111", T1);
    hist_call(&test_new, &anchor_id, "bbbb2222", T2);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path)
}

#[test]
fn at_selects_the_commit_pinned_covering_set() {
    let (_temp, graph) = seed_history();
    let out = query_tests(&graph, "anchor_h", &["--at", "aaaa1111"]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1);
    assert_eq!(lines[1]["name"], "test_old");
    assert_eq!(lines[0]["at_commit"], "aaaa1111");

    let out = query_tests(&graph, "anchor_h", &["--at", "bbbb2222"]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1);
    assert_eq!(lines[1]["name"], "test_new");
}

#[test]
fn as_of_selects_the_latest_commit_at_or_before_the_instant() {
    let (_temp, graph) = seed_history();
    let out = query_tests(&graph, "anchor_h", &["--as-of", T1]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1);
    assert_eq!(lines[1]["name"], "test_old");
}

#[test]
fn at_unknown_commit_is_exit_2() {
    let (_temp, graph) = seed_history();
    let out = query_tests(&graph, "anchor_h", &["--at", "zzzz9999"]).code(2);
    let payload: Value = serde_json::from_slice(&out.get_output().stdout).expect("stdout is JSON");
    assert_eq!(payload["error"]["code"], "missing_commit");
}

// ---------------------------------------------------------------------------
// AC7 — precision probe over a real extractor scan: comments and string
// literals that merely mention the symbol must not produce covering-test rows.
// ---------------------------------------------------------------------------

#[test]
fn scan_mentions_do_not_create_covering_test_rows() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("create src");

    fs::write(
        repo.join("src/lib.rs"),
        r"pub fn process_payment(amount: u64) -> u64 {
    amount * 2
}

pub fn settle_ledger(label: &str) -> usize {
    label.len()
}
",
    )
    .expect("write lib.rs");
    fs::write(
        repo.join("src/payment_tests.rs"),
        r#"#[test]
fn test_refund_flow() {
    // process_payment is mentioned in this comment but never called here.
    let label = "process_payment appears in a string literal only";
    let _ = settle_ledger(label);
}

#[test]
fn test_payment_flow() {
    let _ = process_payment(10);
}
"#,
    )
    .expect("write payment_tests.rs");

    let graph_path = temp.path().join("scan.jsonl");
    egregore()
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The real call is found: one direct covering test. (The extractor
    // qualifies test names with their module.)
    let out = query_tests(&graph_path, "process_payment", &[]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    assert_eq!(lines[0]["total_covering_tests"], 1, "only the real caller");
    let name = lines[1]["name"].as_str().expect("name is a string");
    assert!(
        name.ends_with("test_payment_flow"),
        "direct test row, got: {name}"
    );
    assert_eq!(lines[1]["coverage"], "direct");

    // The mention-only test surfaces only where it really calls.
    let out = query_tests(&graph_path, "settle_ledger", &[]).success();
    let lines = ndjson_lines(&out.get_output().stdout);
    let names: Vec<&str> = lines[1..]
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names.len(), 1, "only the real caller");
    assert!(
        names[0].ends_with("test_refund_flow"),
        "mention-only test row, got: {}",
        names[0]
    );
}

// ---------------------------------------------------------------------------
// Daemon verb tests_for_symbol
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
mod daemon_path {
    use std::process::{Child, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use aletheia_egregore::daemon::{DaemonClient, DaemonQueryRejection, runtime_dir_for_data_dir};
    use tempfile::TempDir;

    /// A running `egregore daemon run` child, killed on drop.
    struct RunningDaemon {
        child: Child,
    }

    impl Drop for RunningDaemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_daemon(data_dir: &std::path::Path) -> (RunningDaemon, DaemonClient) {
        fs::create_dir_all(data_dir).expect("create data dir");
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("egregore"));
        command
            .arg("daemon")
            .arg("run")
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--port")
            .arg("0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("daemon should spawn");

        let metadata_path = runtime_dir_for_data_dir(data_dir).join("egregored.json");
        let start = Instant::now();
        loop {
            if let Ok(contents) = fs::read_to_string(&metadata_path)
                && serde_json::from_str::<Value>(&contents).is_ok()
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "daemon metadata should appear at {}",
                metadata_path.display()
            );
            thread::sleep(Duration::from_millis(50));
        }

        let client =
            DaemonClient::from_data_dir(data_dir).expect("client should connect to the daemon");
        (RunningDaemon { child }, client)
    }

    fn ingest_into_data_dir(store: &std::path::Path, data_dir: &std::path::Path) {
        let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("egregore"))
            .arg("ingest")
            .arg(store)
            .arg("--adapter")
            .arg("embedded")
            .arg("--data-dir")
            .arg(data_dir)
            .status()
            .expect("ingest should run");
        assert!(status.success(), "ingest succeeds");
    }

    fn rejection_code(err: &anyhow::Error) -> String {
        err.downcast_ref::<DaemonQueryRejection>().map_or_else(
            || format!("unrecognized error: {err:?}"),
            |r| r.code.clone(),
        )
    }

    #[test]
    fn daemon_tests_for_symbol_matches_the_cli_answer() {
        let fx = seed();
        let work = TempDir::new().expect("work tempdir");
        let data_dir = work.path().join("daemon-ct");
        ingest_into_data_dir(&fx.graph, &data_dir);
        let (_daemon, client) = start_daemon(&data_dir);

        let result = client
            .query_verb_raw(
                "tests_for_symbol",
                &serde_json::json!({ "handle": "process_order" }),
                None,
            )
            .expect("tests_for_symbol succeeds");
        assert_eq!(result["verb"], "tests_for_symbol");
        assert_eq!(result["tests"]["total_covering_tests"], 2);
        assert_eq!(result["tests"]["direct_tests"], 1);
        assert_eq!(result["tests"]["transitive_tests"], 1);
        let records = result["records"].as_array().expect("records is an array");
        assert_eq!(records.len(), 2);
        let names: Vec<&str> = records
            .iter()
            .map(|r| r["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["test_process_order", "test_end_to_end"]);

        // Five consecutive verb calls render byte-identical output.
        let mut rendered = Vec::new();
        for _ in 0..5 {
            let result = client
                .query_verb_raw(
                    "tests_for_symbol",
                    &serde_json::json!({ "handle": "process_order" }),
                    None,
                )
                .expect("tests_for_symbol succeeds");
            rendered.push(serde_json::to_string(&result).expect("result serializes"));
        }
        for r in &rendered[1..] {
            assert_eq!(r, &rendered[0], "daemon verb output must be deterministic");
        }

        // Error path: unknown handle -> typed no_match rejection.
        let err = client
            .query_verb_raw(
                "tests_for_symbol",
                &serde_json::json!({ "handle": "no_such_symbol" }),
                None,
            )
            .expect_err("unknown handle must fail");
        assert_eq!(rejection_code(&err), "no_match");

        // The CLI --daemon lane routes through the same verb and prints the
        // NDJSON contract.
        let out = egregore()
            .arg("query")
            .arg("tests")
            .arg("process_order")
            .arg("--daemon")
            .arg("--data-dir")
            .arg(&data_dir)
            .assert()
            .success();
        let lines = ndjson_lines(&out.get_output().stdout);
        assert_eq!(lines[0]["total_covering_tests"], 2);
        assert_eq!(lines[0]["ok"], true);
    }
}
