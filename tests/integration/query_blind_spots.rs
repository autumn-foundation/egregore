//! Integration tests for `eg query blind-spots` (issue #265): surface code
//! targets (symbols and/or files) that carry zero cross-domain evidence edges
//! into the agent-memory, verification, or project domains — ranked so the
//! riskiest (most inbound structural references) surface first.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::{
    GraphRecord, NodeKind,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
    },
    scan_repository_at_with_override,
};
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: hand-labeled evidenced vs zero-evidence targets
// ---------------------------------------------------------------------------

/// Crate root. Labels (evidence is appended after the scan):
/// - `entry` — blind, 0 inbound references.
/// - `helper` — blind, 1 inbound reference (called by `entry`).
/// - `lonely` — blind, 0 inbound references.
/// - `noted` — evidenced by an agent-memory Observation (`MENTIONS_SYMBOL`),
///   1 inbound reference (called by `entry`).
/// - `noted_helper` — blind, 1 inbound reference (called by `noted`).
/// - `tested` — evidenced by a verification `TestRun` (`MENTIONS_SYMBOL`),
///   1 inbound reference (called by `entry`).
///   `src/lib.rs` is evidenced by the same `TestRun` (`TOUCHED_FILE`);
///   `src/extra.rs` carries no file-level evidence.
const LIB_RS: &str = r"pub fn entry() -> usize {
    helper() + tested() + noted()
}

fn helper() -> usize {
    2
}

fn lonely() -> usize {
    3
}

pub fn noted() -> usize {
    noted_helper() + 4
}

fn noted_helper() -> usize {
    5
}

pub fn tested() -> usize {
    6
}
";

/// - `planned` — evidenced by a project Task (`MENTIONS_SYMBOL`).
/// - `abandoned` — blind, 0 inbound references.
const EXTRA_RS: &str = r"pub fn planned() -> usize {
    1
}

fn abandoned() -> usize {
    2
}
";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/extra.rs"), EXTRA_RS).expect("extra.rs");
}

/// Record ID of the first record matching `predicate` in `jsonl`.
fn find_record_id(jsonl: &str, predicate: impl Fn(&Value) -> bool) -> String {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("record parses"))
        .find(predicate)
        .expect("fixture must contain the looked-up record")["id"]
        .as_str()
        .expect("record id")
        .to_owned()
}

fn symbol_id(jsonl: &str, name: &str) -> String {
    find_record_id(jsonl, |r| {
        r["kind"] == "Symbol"
            && r["name"]
                .as_str()
                .is_some_and(|n| n == name || n.rsplit("::").next() == Some(name))
    })
}

fn file_id(jsonl: &str, path: &str) -> String {
    find_record_id(jsonl, |r| {
        r["kind"] == "File" && r["repo_relative_path"] == path
    })
}

/// Serializes one record as a JSONL line.
fn line(record: &GraphRecord) -> String {
    let mut s = serde_json::to_string(record).expect("record serializes");
    s.push('\n');
    s
}

/// Creates an edge with the agent-memory domain schema version (v1), since
/// cross-domain evidence labels fall through to the `agent_memory` domain and
/// only v1 is recognized there.
fn evidence_edge(
    label: EdgeLabel,
    source: String,
    target: String,
    confidence: Option<String>,
    summary: String,
) -> GraphRecord {
    let mut record = GraphRecord::edge(label, source, target, confidence, summary);
    if let GraphRecord::Edge { schema_version, .. } = &mut record {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    record
}

fn memory_node(id: &str, kind: NodeKind) -> GraphRecord {
    let mut record = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        None,
        format!("fixture {kind:?} node"),
    );
    // The record ID prefix determines the domain; each domain has its own
    // schema version (all v1 here, not the codegraph SCHEMA_VERSION v9).
    if let GraphRecord::Node { schema_version, .. } = &mut record {
        *schema_version = match kind {
            NodeKind::Observation => AGENT_MEMORY_SCHEMA_VERSION,
            NodeKind::TestRun => VERIFICATION_SCHEMA_VERSION,
            NodeKind::Task => PROJECT_SCHEMA_VERSION,
            _ => *schema_version,
        };
    }
    record
}

/// Appends the labeled cross-domain evidence records to `jsonl` and returns
/// the extended JSONL:
///
/// - `Observation` --`MENTIONS_SYMBOL`--> `noted` (agent-memory evidence)
/// - `TestRun` --`MENTIONS_SYMBOL`--> `tested` (verification evidence)
/// - `TestRun` --`TOUCHED_FILE`--> `src/lib.rs` (verification evidence)
/// - `Task` --`MENTIONS_SYMBOL`--> `planned` (project evidence)
/// - `entry` --`OBSERVES`--> `lonely`, a code-internal negative control: the
///   source node is codegraph-domain, so it must NOT count as evidence.
fn with_evidence(jsonl: &str) -> String {
    let mut out = jsonl.to_owned();
    let obs = "agent_memory:v1:fixture-observation";
    let run = "verification:v1:fixture-test-run";
    let task = "project:v1:fixture-task";

    out.push_str(&line(&memory_node(obs, NodeKind::Observation)));
    out.push_str(&line(&memory_node(run, NodeKind::TestRun)));
    out.push_str(&line(&memory_node(task, NodeKind::Task)));

    let noted = symbol_id(jsonl, "noted");
    let tested = symbol_id(jsonl, "tested");
    let planned = symbol_id(jsonl, "planned");
    let entry = symbol_id(jsonl, "entry");
    let lonely = symbol_id(jsonl, "lonely");
    let lib_file = file_id(jsonl, "src/lib.rs");

    out.push_str(&line(&evidence_edge(
        EdgeLabel::MentionsSymbol,
        obs.to_owned(),
        noted,
        None,
        "observation mentions noted".to_owned(),
    )));
    out.push_str(&line(&evidence_edge(
        EdgeLabel::MentionsSymbol,
        run.to_owned(),
        tested,
        None,
        "test run mentions tested".to_owned(),
    )));
    out.push_str(&line(&evidence_edge(
        EdgeLabel::TouchedFile,
        run.to_owned(),
        lib_file,
        None,
        "test run touched src/lib.rs".to_owned(),
    )));
    out.push_str(&line(&evidence_edge(
        EdgeLabel::MentionsSymbol,
        task.to_owned(),
        planned,
        None,
        "task mentions planned".to_owned(),
    )));
    // Negative control: same label family, wrong source domain — `lonely`
    // must stay a blind spot.
    out.push_str(&line(&evidence_edge(
        EdgeLabel::Observes,
        entry,
        lonely,
        None,
        "code-internal observes edge (not evidence)".to_owned(),
    )));
    out
}

/// Scans the labeled fixture, appends the evidence records, and writes the
/// JSONL graph. Returns (`TempDir`, graph path). Caller must keep the
/// `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let scanned = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("blind-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let jsonl = with_evidence(&scanned);
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_blind_spots(graph: &Path, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "blind-spots", "--graph"])
        .arg(graph)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn blind_spot_names(parsed: &Value) -> Vec<String> {
    parsed["blind_spots"]
        .as_array()
        .expect("blind_spots array")
        .iter()
        .map(|c| c["name"].as_str().expect("name").to_owned())
        .collect()
}

fn blind_spot<'a>(parsed: &'a Value, name: &str) -> &'a Value {
    parsed["blind_spots"]
        .as_array()
        .expect("blind_spots array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("missing blind spot named {name}"))
}

fn diagnostic_codes(parsed: &Value) -> Vec<String> {
    parsed["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().expect("code").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 + success metric: exactly the labeled zero-evidence set, ranked
// riskiest-first, deterministic across runs
// ---------------------------------------------------------------------------

#[test]
fn blind_spots_returns_exactly_the_labeled_zero_evidence_set() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    // Ranked by inbound structural reference count descending, ties broken
    // by (path, start_line, record_id): helper(1) and noted_helper(1) first,
    // then the 0-count ties: src/extra.rs (file, no span) before
    // extra::abandoned (same path, has span), then src/lib.rs symbols.
    let names = blind_spot_names(&parsed);
    assert_eq!(
        names,
        vec![
            "helper",
            "noted_helper",
            "src/extra.rs",
            "extra::abandoned",
            "entry",
            "lonely",
        ],
        "recall and precision must both be 100% against the label set, ranked riskiest-first"
    );
}

#[test]
fn evidenced_targets_are_never_reported_as_blind_spots() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);
    let names = blind_spot_names(&parsed);

    for evidenced in ["noted", "tested", "planned"] {
        assert!(
            !names.contains(&evidenced.to_owned()),
            "{evidenced} carries a qualifying cross-domain evidence edge and must not be a blind spot"
        );
    }
    assert!(
        !names.contains(&"src/lib.rs".to_owned()),
        "src/lib.rs is touched by a verification run and must not be a blind spot"
    );
}

#[test]
fn code_internal_observes_edge_does_not_count_as_evidence() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    assert!(
        blind_spot_names(&parsed).contains(&"lonely".to_owned()),
        "an OBSERVES edge from a codegraph-domain node is not cross-domain \
         evidence; lonely must stay a blind spot"
    );
}

// ---------------------------------------------------------------------------
// AC2 + AC3: citable handles and the ranking key on every row
// ---------------------------------------------------------------------------

#[test]
fn rows_carry_citation_fields_and_the_ranking_key() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    let rows = parsed["blind_spots"].as_array().expect("blind_spots array");
    assert!(!rows.is_empty(), "fixture must yield blind spots");
    for row in rows {
        assert!(row["record_id"].as_str().is_some(), "row needs record_id");
        assert!(
            row["schema_version"].as_u64().is_some(),
            "row needs schema_version"
        );
        assert!(row["name"].as_str().is_some(), "row needs name");
        assert!(
            row["target_kind"].as_str().is_some(),
            "row needs target_kind"
        );
        assert!(
            row["repo_relative_path"]
                .as_str()
                .is_some_and(|p| p.starts_with("src/")),
            "row needs a repo-relative file handle"
        );
        assert!(
            row["inbound_reference_count"].as_u64().is_some(),
            "row needs the ranking key"
        );
    }
    // Symbol rows carry spans; the file row cites the path handle.
    assert!(
        blind_spot(&parsed, "helper")["span"]["start_line"]
            .as_u64()
            .is_some(),
        "symbol rows carry a span handle"
    );
    assert_eq!(blind_spot(&parsed, "src/extra.rs")["target_kind"], "file");
}

#[test]
fn ranking_is_prominence_descending_then_deterministic() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    let prominences: Vec<u64> = parsed["blind_spots"]
        .as_array()
        .expect("blind_spots array")
        .iter()
        .map(|r| r["inbound_reference_count"].as_u64().expect("count"))
        .collect();
    let mut sorted = prominences.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        prominences, sorted,
        "rows must be ranked by inbound structural reference count descending"
    );
    assert_eq!(
        blind_spot(&parsed, "helper")["inbound_reference_count"],
        1,
        "helper is called once by entry"
    );
    assert_eq!(
        blind_spot(&parsed, "entry")["inbound_reference_count"],
        0,
        "entry has no inbound references"
    );
}

// ---------------------------------------------------------------------------
// AC1: the evidence edge classes are enumerated in the response
// ---------------------------------------------------------------------------

#[test]
fn response_enumerates_the_evidence_edge_classes() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    let classes: Vec<&str> = parsed["evidence_edge_classes"]
        .as_array()
        .expect("evidence_edge_classes")
        .iter()
        .map(|c| c.as_str().expect("class"))
        .collect();
    for required in [
        "OBSERVES",
        "MENTIONS_SYMBOL",
        "TOUCHED_FILE",
        "FAILED_ON",
        "TOUCHES_FILE",
    ] {
        assert!(
            classes.contains(&required),
            "documented evidence class {required} must be listed"
        );
    }
}

// ---------------------------------------------------------------------------
// AC4: --kind symbol|file filter; AC: evaluated-target count reported
// ---------------------------------------------------------------------------

#[test]
fn kind_filter_restricts_the_evaluated_population() {
    let (_temp, graph) = fixture_graph();

    let symbols = run_blind_spots(&graph, &["--kind", "symbol"]);
    let symbol_names = blind_spot_names(&symbols);
    assert!(
        symbol_names.iter().all(|n| n != "src/extra.rs"),
        "--kind symbol must not return file rows"
    );
    assert!(
        symbol_names.contains(&"lonely".to_owned()),
        "--kind symbol must still return symbol rows"
    );

    let files = run_blind_spots(&graph, &["--kind", "file"]);
    let file_names = blind_spot_names(&files);
    assert_eq!(
        file_names,
        vec!["src/extra.rs"],
        "--kind file must return only file rows"
    );
    // Two files are evaluated: src/lib.rs has TOUCHED_FILE evidence (not blind),
    // src/extra.rs is blind.
    assert_eq!(
        files["counts"]["targets_evaluated"].as_u64(),
        Some(2),
        "both fixture files are evaluated"
    );
    assert_eq!(
        files["counts"]["targets_with_evidence"].as_u64(),
        Some(1),
        "src/lib.rs has TOUCHED_FILE evidence"
    );
}

#[test]
fn counts_report_evaluated_targets_evidence_and_blind_spots() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    let counts = &parsed["counts"];
    assert!(
        counts["targets_evaluated"].as_u64().is_some_and(|n| n > 0),
        "the count of evaluated targets must be reported"
    );
    assert_eq!(
        counts["targets_evaluated"].as_u64().expect("evaluated"),
        counts["targets_with_evidence"].as_u64().expect("evidenced")
            + counts["blind_spots"].as_u64().expect("blind"),
        "evaluated = evidenced + blind"
    );
    assert_eq!(
        counts["blind_spots"].as_u64().expect("blind"),
        parsed["blind_spots"].as_array().expect("rows").len() as u64
    );
}

// ---------------------------------------------------------------------------
// AC5: empty result set is explicit data, not an error
// ---------------------------------------------------------------------------

#[test]
fn fully_evidenced_store_reports_empty_blind_spots_explicitly() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let scanned = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("blind-full"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");

    // Evidence for EVERY symbol and file: one observation mentioning each
    // symbol, one test run touching each file.
    let mut jsonl = scanned.clone();
    let obs = "agent_memory:v1:fixture-observation";
    let run = "verification:v1:fixture-test-run";
    jsonl.push_str(&line(&memory_node(obs, NodeKind::Observation)));
    jsonl.push_str(&line(&memory_node(run, NodeKind::TestRun)));
    for name in [
        "entry",
        "helper",
        "lonely",
        "noted",
        "noted_helper",
        "tested",
        "planned",
        "abandoned",
    ] {
        let target = symbol_id(&scanned, name);
        jsonl.push_str(&line(&evidence_edge(
            EdgeLabel::MentionsSymbol,
            obs.to_owned(),
            target,
            None,
            format!("observation mentions {name}"),
        )));
    }
    for path in ["src/lib.rs", "src/extra.rs"] {
        let target = file_id(&scanned, path);
        jsonl.push_str(&line(&evidence_edge(
            EdgeLabel::TouchedFile,
            run.to_owned(),
            target,
            None,
            format!("test run touched {path}"),
        )));
    }
    let graph = temp.path().join("full.graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");

    let parsed = run_blind_spots(&graph, &[]);
    assert_eq!(parsed["ok"], true, "an empty blind-spot set is a success");
    assert_eq!(
        parsed["blind_spots"],
        serde_json::json!([]),
        "absence is data: an explicit empty array, not an error"
    );
    let codes = diagnostic_codes(&parsed);
    assert!(
        codes.contains(&"no_blind_spots".to_owned()),
        "a fully-evidenced store must carry the distinct no_blind_spots signal"
    );
    assert!(
        parsed["counts"]["targets_evaluated"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "coverage stays interpretable: the evaluated count is still reported"
    );
}

// ---------------------------------------------------------------------------
// AC6: a target that gains evidence disappears in the very next query
// ---------------------------------------------------------------------------

#[test]
fn newly_linked_evidence_removes_the_target_on_the_next_query() {
    let (temp, graph) = fixture_graph();
    let before = run_blind_spots(&graph, &[]);
    assert!(
        blind_spot_names(&before).contains(&"lonely".to_owned()),
        "lonely starts as a blind spot"
    );

    // Link an observation to `lonely` after the first query.
    let jsonl = fs::read_to_string(&graph).expect("read graph");
    let lonely = symbol_id(&jsonl, "lonely");
    let obs = "agent_memory:v1:late-observation";
    let mut appended = jsonl;
    appended.push_str(&line(&memory_node(obs, NodeKind::Observation)));
    appended.push_str(&line(&evidence_edge(
        EdgeLabel::MentionsSymbol,
        obs.to_owned(),
        lonely,
        None,
        "late observation mentions lonely".to_owned(),
    )));
    fs::write(&graph, appended).expect("append evidence");
    // Keep `temp` alive: dropping it would delete the graph file before the
    // second query runs.
    let _keep = temp;

    let after = run_blind_spots(&graph, &[]);
    assert!(
        !blind_spot_names(&after).contains(&"lonely".to_owned()),
        "a target that gains a qualifying evidence edge must disappear in the \
         very next query"
    );
}

// ---------------------------------------------------------------------------
// AC: --format text renders one line per blind spot
// ---------------------------------------------------------------------------

#[test]
fn text_format_renders_one_line_per_blind_spot() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["query", "blind-spots", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");

    for name in ["helper", "entry", "lonely", "src/extra.rs"] {
        assert!(
            stdout.contains(name),
            "text output must name the blind spot {name}"
        );
    }
    assert!(
        !stdout.contains("\"blind_spots\""),
        "text format must not be JSON"
    );
}

// ---------------------------------------------------------------------------
// AC4: --repo scope filter; unknown selector exits non-zero, machine-readable
// ---------------------------------------------------------------------------

#[test]
fn repo_scope_limits_targets_to_one_repository() {
    let temp_a = tempfile::tempdir().expect("temp dir a");
    fs::create_dir_all(temp_a.path().join("src")).expect("src dir");
    fs::write(temp_a.path().join("src/lib.rs"), "fn alpha_only() {}\n").expect("lib.rs");
    let temp_b = tempfile::tempdir().expect("temp dir b");
    fs::create_dir_all(temp_b.path().join("src")).expect("src dir");
    fs::write(temp_b.path().join("src/lib.rs"), "fn beta_only() {}\n").expect("lib.rs");

    let jsonl_a = scan_repository_at_with_override(temp_a.path(), FIXED_TIME, Some("repo-alpha"))
        .expect("scan a")
        .to_jsonl()
        .expect("serialize a");
    let jsonl_b = scan_repository_at_with_override(temp_b.path(), FIXED_TIME, Some("repo-beta"))
        .expect("scan b")
        .to_jsonl()
        .expect("serialize b");
    let graph = temp_a.path().join("merged.graph.jsonl");
    fs::write(&graph, format!("{jsonl_a}{jsonl_b}")).expect("write merged graph");

    let output = egregore()
        .args(["query", "blind-spots", "--repo", "repo-alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON");
    let names = blind_spot_names(&parsed);
    assert!(names.contains(&"alpha_only".to_owned()));
    assert!(
        !names.contains(&"beta_only".to_owned()),
        "--repo must exclude the other repository's targets"
    );
}

#[test]
fn unknown_repo_selector_is_rejected_with_exit_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args(["query", "blind-spots", "--repo", "nope", "--graph"])
        .arg(&graph)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Success metric: byte-stable across repeated runs, read-only
// ---------------------------------------------------------------------------

fn dir_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                entries.push((
                    path.to_string_lossy().into_owned(),
                    fs::read(&path).expect("read file"),
                ));
            }
        }
    }
    entries.sort();
    entries
}

#[test]
fn query_is_read_only_and_byte_identical_across_five_runs() {
    let (temp, graph) = fixture_graph();
    let before = dir_snapshot(temp.path());

    let mut outputs: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        let output = egregore()
            .args(["query", "blind-spots", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(output);
    }
    for run in &outputs[1..] {
        assert_eq!(
            run, &outputs[0],
            "output must be byte-identical across consecutive runs"
        );
    }

    let after = dir_snapshot(temp.path());
    assert_eq!(
        before, after,
        "the query must create, modify, and delete nothing"
    );
}

// ---------------------------------------------------------------------------
// Every row resolves to a valid repo-relative file/span handle
// ---------------------------------------------------------------------------

#[test]
fn every_row_resolves_to_a_valid_file_span_handle() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_blind_spots(&graph, &[]);

    let known_files: Vec<String> = ["src/lib.rs", "src/extra.rs"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for row in parsed["blind_spots"].as_array().expect("rows") {
        let path = row["repo_relative_path"].as_str().expect("path");
        assert!(
            known_files.contains(&path.to_owned()),
            "row path {path} must resolve to a scanned file"
        );
        if row["target_kind"] == "symbol" {
            assert!(
                row["span"]["start_line"].as_u64().is_some_and(|n| n >= 1),
                "symbol rows must carry a valid span"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// --data-dir reads leave the live embedded store byte-for-byte untouched
// ---------------------------------------------------------------------------

/// Recursive path -> content map for byte-exact store comparisons.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_contents(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut contents = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("dir should read") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = fs::read(&path).expect("file should read");
                contents.insert(path.display().to_string(), bytes);
            }
        }
    }
    contents
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn blind_spots_query_is_read_only_for_embedded_store() {
    let (temp, graph) = fixture_graph();
    let data_dir = temp.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let bytes_before = dir_contents(&data_dir);

    egregore()
        .args(["query", "blind-spots", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    assert_eq!(
        dir_contents(&data_dir),
        bytes_before,
        "querying must leave the live embedded store byte-for-byte untouched"
    );
}
