//! End-to-end tests for `eg query transitive-callers <handle>` (issue #139):
//! bounded, deterministic transitive inbound reachability with call paths.
#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, SCHEMA_VERSION, stable_id},
};
use assert_cmd::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn symbol(graph: &mut Graph, path: &str, name: &str, lines: (usize, usize)) -> String {
    let id = sym_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(lines.0, lines.1),
        name.to_owned(),
        "rust",
        format!("fn {name} in {path}"),
    ));
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

fn calls(graph: &mut Graph, from: &str, to: &str, resolution: Option<CallResolution>) {
    let mut edge = GraphRecord::edge(
        EdgeLabel::Calls,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        "call edge".to_owned(),
    );
    if let Some(r) = resolution {
        edge = edge.with_resolution(r);
    }
    graph.push(edge);
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
// Fixture — >= 20 symbols, CALLS/REFERENCES chains of depth >= 3, one
// mutual-recursion cycle, one unrelated same-name pair, and a symbol reachable
// only via a >= 2-hop path (AC1).
// ---------------------------------------------------------------------------

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    anchor_id: String,
    caller_direct1_id: String,
    caller_direct2_id: String,
    mid_id: String,
    entry_id: String,
    top_id: String,
    deep_amb_id: String,
    ref_sym_id: String,
    ref_ref_id: String,
    cycle_x_id: String,
    cycle_y_id: String,
    dup_a_id: String,
    dup_b_id: String,
    dup_a_caller_id: String,
    dup_b_caller_id: String,
    tombstoned_id: String,
}

#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("transitive_callers.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-tc"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-tc".to_owned()),
        "Repository repo-tc".to_owned(),
    ));

    for p in [
        "src/anchor.rs",
        "src/mid.rs",
        "src/entry.rs",
        "src/cycle.rs",
        "src/dup_a.rs",
        "src/dup_b.rs",
        "src/refs.rs",
        "src/pad.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // Target and direct callers.
    let anchor_id = symbol(&mut graph, "src/anchor.rs", "anchor_fn", (5, 20));
    let caller_direct1_id = symbol(&mut graph, "src/mid.rs", "caller_direct1", (5, 12));
    let caller_direct2_id = symbol(&mut graph, "src/mid.rs", "caller_direct2", (14, 22));
    calls(
        &mut graph,
        &caller_direct1_id,
        &anchor_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &caller_direct2_id,
        &anchor_id,
        Some(CallResolution::Ambiguous),
    );

    // Depth-3 CALLS chain: top_fn -> entry_fn -> mid_fn -> anchor_fn.
    let mid_id = symbol(&mut graph, "src/mid.rs", "mid_fn", (24, 40));
    let entry_id = symbol(&mut graph, "src/entry.rs", "entry_fn", (5, 20));
    let top_id = symbol(&mut graph, "src/entry.rs", "top_fn", (22, 30));
    calls(
        &mut graph,
        &mid_id,
        &anchor_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &entry_id,
        &mid_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &top_id,
        &entry_id,
        Some(CallResolution::Resolved),
    );

    // Ambiguous edge two hops out: deep_amb -(ambiguous)-> mid_fn -(resolved)-> anchor_fn.
    let deep_amb_id = symbol(&mut graph, "src/entry.rs", "deep_amb", (32, 40));
    calls(
        &mut graph,
        &deep_amb_id,
        &mid_id,
        Some(CallResolution::Ambiguous),
    );

    // REFERENCES chain (no resolution field on these edges):
    // ref_ref -> ref_sym -> anchor_fn.
    let ref_sym_id = symbol(&mut graph, "src/refs.rs", "ref_sym", (5, 12));
    let ref_ref_id = symbol(&mut graph, "src/refs.rs", "ref_ref", (14, 20));
    references(&mut graph, &ref_sym_id, &anchor_id);
    references(&mut graph, &ref_ref_id, &ref_sym_id);

    // Mutual-recursion cycle: cycle_x <-> cycle_y, cycle_x -> anchor_fn.
    let cycle_x_id = symbol(&mut graph, "src/cycle.rs", "cycle_x", (5, 12));
    let cycle_y_id = symbol(&mut graph, "src/cycle.rs", "cycle_y", (14, 20));
    calls(
        &mut graph,
        &cycle_x_id,
        &anchor_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &cycle_x_id,
        &cycle_y_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &cycle_y_id,
        &cycle_x_id,
        Some(CallResolution::Resolved),
    );

    // Unrelated same-name pair with their own callers (zero-bleed fixture).
    let dup_a_id = symbol(&mut graph, "src/dup_a.rs", "dup_name", (5, 12));
    let dup_b_id = symbol(&mut graph, "src/dup_b.rs", "dup_name", (5, 12));
    let dup_a_caller_id = symbol(&mut graph, "src/dup_a.rs", "dup_a_caller", (14, 20));
    let dup_b_caller_id = symbol(&mut graph, "src/dup_b.rs", "dup_b_caller", (14, 20));
    calls(
        &mut graph,
        &dup_a_caller_id,
        &dup_a_id,
        Some(CallResolution::Ambiguous),
    );
    calls(
        &mut graph,
        &dup_b_caller_id,
        &dup_b_id,
        Some(CallResolution::Ambiguous),
    );

    // Padding symbols with no edges to anchor_fn (>= 20 symbols total).
    for (i, name) in ["pad1", "pad2", "pad3", "pad4", "pad5"].iter().enumerate() {
        symbol(&mut graph, "src/pad.rs", name, (5 + i * 10, 12 + i * 10));
    }

    // Dangling inbound CALLS edge from a missing source record.
    let missing_source_id = format!("codegraph:v{SCHEMA_VERSION}:{}", "c".repeat(64));
    graph.push(GraphRecord::edge(
        EdgeLabel::Calls,
        missing_source_id,
        anchor_id.clone(),
        Some("1.0".to_owned()),
        "missing source calls anchor_fn (dangling)".to_owned(),
    ));

    // Tombstoned symbol for the stale_handle test.
    let tombstoned_id = symbol(&mut graph, "src/pad.rs", "deleted_fn", (60, 70));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &tombstoned_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: tombstoned_id.clone(),
        summary: "deleted_fn was deleted".to_owned(),
        producer: None,
    });

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    Fixture {
        _temp: temp,
        graph: path,
        anchor_id,
        caller_direct1_id,
        caller_direct2_id,
        mid_id,
        entry_id,
        top_id,
        deep_amb_id,
        ref_sym_id,
        ref_ref_id,
        cycle_x_id,
        cycle_y_id,
        dup_a_id,
        dup_b_id,
        dup_a_caller_id,
        dup_b_caller_id,
        tombstoned_id,
    }
}

/// Runs the query and parses the NDJSON output into (header, rows).
fn run_query(args: &[&str]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let stdout = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse_ndjson(&stdout)
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

fn row_ids(rows: &[serde_json::Value]) -> Vec<&str> {
    rows.iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect()
}

fn row_by_id<'a>(rows: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["record_id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("row {id} missing"))
}

// ---------------------------------------------------------------------------
// AC1/AC3 — transitive reachable set with hop distances and concrete paths
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn reaches_transitive_callers_with_paths() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["direction"], "inbound");
    assert_eq!(
        header["target"]["record_id"].as_str(),
        Some(f.anchor_id.as_str())
    );
    assert_eq!(header["max_depth"].as_u64(), Some(5), "safe default bound");
    let disclaimer = header["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("not proof"),
        "disclaimer must state leads-not-proof; got {disclaimer:?}"
    );
    assert_eq!(
        usize::try_from(header["total_reachable"].as_u64().unwrap()).unwrap(),
        rows.len(),
        "header count must match emitted rows"
    );

    let ids = row_ids(&rows);
    let expected = [
        &f.caller_direct1_id,
        &f.caller_direct2_id,
        &f.mid_id,
        &f.ref_sym_id,
        &f.cycle_x_id,
        &f.entry_id,
        &f.deep_amb_id,
        &f.cycle_y_id,
        &f.ref_ref_id,
        &f.top_id,
    ];
    for id in expected {
        assert!(ids.contains(&id.as_str()), "missing reachable row {id}");
    }
    assert_eq!(ids.len(), expected.len(), "no false positives: got {ids:?}");

    // Hop distances (shortest paths).
    assert_eq!(row_by_id(&rows, &f.caller_direct1_id)["hop"], 1);
    assert_eq!(row_by_id(&rows, &f.mid_id)["hop"], 1);
    assert_eq!(row_by_id(&rows, &f.ref_sym_id)["hop"], 1);
    assert_eq!(row_by_id(&rows, &f.entry_id)["hop"], 2);
    assert_eq!(row_by_id(&rows, &f.ref_ref_id)["hop"], 2);
    assert_eq!(row_by_id(&rows, &f.top_id)["hop"], 3);

    // Every row carries the citable fields plus a concrete connecting path.
    for row in &rows {
        assert!(row["record_id"].as_str().is_some(), "record_id required");
        assert!(row["schema_version"].is_number(), "schema_version required");
        assert!(row["name"].as_str().is_some(), "name required");
        assert!(row["kind"].as_str().is_some(), "kind required");
        assert!(
            row["repo_relative_path"].as_str().is_some(),
            "repo_relative_path required"
        );
        assert!(row["span"].is_object(), "span required");
        assert!(row["hop"].is_number(), "hop required");
        let path = row["path"].as_array().expect("path array");
        assert_eq!(
            path.len() as u64,
            row["hop"].as_u64().unwrap(),
            "path length equals hop distance"
        );
        // Path is an ordered chain from this row down to the target.
        assert_eq!(
            path.first().unwrap()["source_record_id"].as_str(),
            row["record_id"].as_str(),
            "first path step starts at the row's own record"
        );
        assert_eq!(
            path.last().unwrap()["target_record_id"].as_str(),
            Some(f.anchor_id.as_str()),
            "last path step ends at the queried target"
        );
        for step in path {
            assert!(step["edge_record_id"].as_str().is_some(), "edge handle");
            assert!(step["edge_label"].as_str().is_some(), "edge label");
        }
        // Consecutive steps chain: step[i].target == step[i+1].source.
        for pair in path.windows(2) {
            assert_eq!(
                pair[0]["target_record_id"], pair[1]["source_record_id"],
                "path steps must chain contiguously"
            );
        }
    }

    // top_fn's 3-hop path names the exact chain.
    let top_path = row_by_id(&rows, &f.top_id)["path"].as_array().unwrap();
    let chain: Vec<&str> = top_path
        .iter()
        .map(|s| s["source_record_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        chain,
        vec![f.top_id.as_str(), f.entry_id.as_str(), f.mid_id.as_str()]
    );

    // Dangling inbound edge surfaces as a diagnostic, never a row.
    let diag_codes: Vec<&str> = header["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(
        diag_codes.contains(&"unresolved_edge_source"),
        "dangling edge must yield unresolved_edge_source; got {diag_codes:?}"
    );
}

// ---------------------------------------------------------------------------
// Resolution semantics (issues #152/#134) propagate along paths: a path is as
// weak as its weakest labeled CALLS edge.
// ---------------------------------------------------------------------------

#[test]
fn resolution_propagates_weakest_along_path() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    // Fully resolved chain.
    assert_eq!(row_by_id(&rows, &f.top_id)["path_resolution"], "resolved");
    assert_eq!(row_by_id(&rows, &f.mid_id)["path_resolution"], "resolved");

    // Direct ambiguous edge.
    assert_eq!(
        row_by_id(&rows, &f.caller_direct2_id)["path_resolution"],
        "ambiguous"
    );

    // Ambiguous edge two hops out weakens the whole path.
    let deep = row_by_id(&rows, &f.deep_amb_id);
    assert_eq!(deep["path_resolution"], "ambiguous");
    let labels: Vec<Option<&str>> = deep["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["resolution"].as_str())
        .collect();
    assert_eq!(
        labels,
        vec![Some("ambiguous"), Some("resolved")],
        "per-step resolution must be exposed"
    );

    // REFERENCES edges carry no resolution contract: field absent.
    let ref_row = row_by_id(&rows, &f.ref_sym_id);
    assert!(
        ref_row.get("path_resolution").is_none(),
        "a path with no labeled CALLS edge must omit path_resolution; got {ref_row}"
    );
}

// ---------------------------------------------------------------------------
// AC4 — --max-depth bound, truncation diagnostic with dropped counts per
// depth, and depth-1 equivalence with the direct-callers set (#122 -> #76).
// ---------------------------------------------------------------------------

#[test]
fn max_depth_bounds_walk_and_reports_dropped_frontier() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
        "--max-depth",
        "2",
    ]);

    let ids = row_ids(&rows);
    assert!(
        !ids.contains(&f.top_id.as_str()),
        "top_fn is beyond depth 2"
    );
    assert!(ids.contains(&f.entry_id.as_str()), "entry_fn is at depth 2");

    let trunc = &header["truncation"];
    assert!(
        trunc.is_object(),
        "truncation diagnostic required: {header}"
    );
    assert_eq!(trunc["max_depth"].as_u64(), Some(2));
    let dropped = trunc["dropped_frontier"]
        .as_array()
        .expect("dropped_frontier");
    assert_eq!(
        dropped.len(),
        1,
        "one dropped depth expected; got {dropped:?}"
    );
    assert_eq!(dropped[0]["depth"].as_u64(), Some(3));
    assert_eq!(
        dropped[0]["count"].as_u64(),
        Some(1),
        "top_fn dropped at depth 3"
    );
    assert_eq!(trunc["dropped_total"].as_u64(), Some(1));
}

#[test]
fn no_truncation_when_walk_exhausts_graph() {
    let f = seed();
    let (header, _) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert!(
        header.get("truncation").is_none() || header["truncation"].is_null(),
        "no truncation when the graph is exhausted within the bound: {header}"
    );
}

#[test]
fn max_depth_1_equals_direct_callers_set() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
        "--max-depth",
        "1",
    ]);
    let mut ids = row_ids(&rows);
    ids.sort_unstable();

    // The depth-1 result is exactly the direct inbound CALLS/REFERENCES
    // neighbor set that change-impact (#76, the canonical home of #122)
    // reports as direct_callers + inbound referencing rows.
    let stdout = egregore()
        .args([
            "query",
            "change-impact",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    let mut expected: Vec<String> = v["direct_callers"]
        .as_array()
        .unwrap()
        .iter()
        .chain(
            v["referencing_files"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["direction"] == "inbound" && r["relation"] == "REFERENCES"),
        )
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();
    expected.sort_unstable();
    expected.dedup();

    assert_eq!(
        ids, expected,
        "depth-1 set must match direct callers (no drift)"
    );
}

#[test]
fn max_depth_zero_rejected_exit1() {
    let f = seed();
    egregore()
        .args([
            "query",
            "transitive-callers",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--max-depth",
            "0",
        ])
        .assert()
        .code(1);
}

#[test]
fn huge_max_depth_terminates_and_matches_exhausting_depth() {
    let f = seed();
    let run = |depth: &str| -> Vec<u8> {
        egregore()
            .args([
                "query",
                "transitive-callers",
                &f.anchor_id,
                "--graph",
                f.graph.to_str().unwrap(),
                "--max-depth",
                depth,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let small = run("10");
    let huge = run("1000000000");
    // Everything but the max_depth header field must be identical.
    let (h_small, r_small) = parse_ndjson(&small);
    let (h_huge, r_huge) = parse_ndjson(&huge);
    assert_eq!(
        r_small, r_huge,
        "rows must match once the graph is exhausted"
    );
    assert_eq!(h_small["total_reachable"], h_huge["total_reachable"]);
}

// ---------------------------------------------------------------------------
// AC5 — cycle handling: reported once with the shortest discovered path,
// traversal terminates.
// ---------------------------------------------------------------------------

#[test]
fn cycle_terminates_and_reports_each_symbol_once() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    let x_rows = rows
        .iter()
        .filter(|r| r["record_id"].as_str() == Some(f.cycle_x_id.as_str()))
        .count();
    let y_rows = rows
        .iter()
        .filter(|r| r["record_id"].as_str() == Some(f.cycle_y_id.as_str()))
        .count();
    assert_eq!(x_rows, 1, "cycle member reported exactly once");
    assert_eq!(y_rows, 1, "cycle member reported exactly once");

    assert_eq!(row_by_id(&rows, &f.cycle_x_id)["hop"], 1);
    assert_eq!(
        row_by_id(&rows, &f.cycle_y_id)["hop"],
        2,
        "cycle_y's shortest path is via cycle_x"
    );

    // The target itself is never resurfaced as its own transitive caller.
    assert!(
        !row_ids(&rows).contains(&f.anchor_id.as_str()),
        "anchor must not appear in its own reachable set"
    );
}

// ---------------------------------------------------------------------------
// AC2 — handle resolution: ambiguous names list all candidates (exit 1),
// unknown handles are a stable no_match (exit 2), record IDs disambiguate
// with zero same-name bleed.
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_name_exit1_lists_all_candidates() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "transitive-callers",
            "dup_name",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let err = String::from_utf8(stderr).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(err.trim()).expect("stderr JSON");
    let candidates = v["Ambiguous"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("Ambiguous.candidates required; got {v}"));
    let ids: Vec<&str> = candidates.iter().filter_map(|c| c.as_str()).collect();
    assert!(
        ids.contains(&f.dup_a_id.as_str()),
        "candidate A missing: {ids:?}"
    );
    assert!(
        ids.contains(&f.dup_b_id.as_str()),
        "candidate B missing: {ids:?}"
    );
}

#[test]
fn record_id_disambiguates_with_zero_same_name_bleed() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.dup_a_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let ids = row_ids(&rows);
    assert!(
        ids.contains(&f.dup_a_caller_id.as_str()),
        "dup_a's own caller must be reachable"
    );
    assert!(
        !ids.contains(&f.dup_b_caller_id.as_str()),
        "the unrelated same-name symbol's caller must never bleed in"
    );
    assert!(
        !ids.contains(&f.dup_b_id.as_str()),
        "no bleed of the twin itself"
    );
}

#[test]
fn unique_name_resolves_like_symbol_query() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        "anchor_fn",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(
        header["target"]["record_id"].as_str(),
        Some(f.anchor_id.as_str())
    );
    assert!(!rows.is_empty());
}

#[test]
fn unknown_handle_no_match_exit2() {
    let f = seed();
    let absent = format!("codegraph:v{}:{}", SCHEMA_VERSION, "b".repeat(64));
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &absent,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match");
}

#[test]
fn stale_handle_exit2() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &f.tombstoned_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "stale_handle");
}

#[test]
fn malformed_id_exit1() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "transitive-callers",
            "codegraph:v1:zzz",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stderr).expect("utf8").trim()).expect("json");
    assert!(v.get("Unsupported").is_some(), "got {v}");
}

#[test]
fn empty_handle_exit1() {
    let f = seed();
    egregore()
        .args(["query", "transitive-callers", "", "--graph"])
        .arg(&f.graph)
        .assert()
        .code(1);
}

#[test]
fn file_handle_rejected_exit1() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "transitive-callers",
            "src/anchor.rs",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stderr).expect("utf8").trim()).expect("json");
    assert!(
        v.get("Unsupported").is_some(),
        "file handles are out of scope for transitive-callers; got {v}"
    );
}

// ---------------------------------------------------------------------------
// AC2 — a symbol with no inbound edges is an explicit empty result, exit 0.
// ---------------------------------------------------------------------------

#[test]
fn no_inbound_edges_is_explicit_empty_result_exit0() {
    let f = seed();
    let pad_id = sym_id("src/pad.rs", "pad1");
    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        &pad_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["ok"], true);
    assert_eq!(header["total_reachable"].as_u64(), Some(0));
    assert!(rows.is_empty(), "no rows for an uncalled symbol");
}

// ---------------------------------------------------------------------------
// AC7 — byte-identical output across 5 repeated runs.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_output_x5() {
    let f = seed();
    let run = || -> Vec<u8> {
        egregore()
            .args([
                "query",
                "transitive-callers",
                &f.anchor_id,
                "--graph",
                f.graph.to_str().unwrap(),
                "--max-depth",
                "2",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    for i in 2..=5 {
        assert_eq!(first, run(), "run {i} not byte-identical");
    }
}

// ---------------------------------------------------------------------------
// Canonical row ordering: (hop ascending, record_id ascending).
// ---------------------------------------------------------------------------

#[test]
fn rows_sorted_by_hop_then_record_id() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let keys: Vec<(u64, &str)> = rows
        .iter()
        .map(|r| (r["hop"].as_u64().unwrap(), r["record_id"].as_str().unwrap()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "rows must be canonically ordered");
}

// ---------------------------------------------------------------------------
// --format text mode.
// ---------------------------------------------------------------------------

#[test]
fn text_format_lists_reachable_symbols() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--format",
            "text",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(
        out.contains("top_fn"),
        "text mode must list reachable symbols: {out}"
    );
    assert!(
        out.contains("hop"),
        "text mode must show hop distances: {out}"
    );
}

// ---------------------------------------------------------------------------
// Redaction: output never includes raw source text or payloads — record IDs,
// names, paths, spans, and hashes only. Spot-check that no summary body text
// from the fixture leaks.
// ---------------------------------------------------------------------------

#[test]
fn output_carries_handles_not_payloads() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(
        !out.contains("Source file src/"),
        "node summaries (potential payload text) must not be emitted"
    );
}

// ---------------------------------------------------------------------------
// eg alias.
// ---------------------------------------------------------------------------

#[test]
fn eg_alias_works() {
    let f = seed();
    eg().args(["query", "transitive-callers", &f.anchor_id, "--graph"])
        .arg(&f.graph)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// AC6 — temporal selectors --at / --as-of over a history graph.
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

/// History fixture: at c1 `caller_a` calls `anchor_h`; at c2 that call is gone
/// and `caller_b` calls `anchor_h` instead.
fn seed_history() -> (tempfile::TempDir, PathBuf, String, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");
    let mut graph = Graph::new();

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

    let hist_symbol = |name: &str, commit: &str, vt: &str| -> (String, GraphRecord) {
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
        (id, rec)
    };

    let (anchor_id, a1) = hist_symbol("anchor_h", "aaaa1111", T1);
    let (caller_a_id, c1) = hist_symbol("caller_a", "aaaa1111", T1);
    graph.push(a1);
    graph.push(c1);
    let (_, a2) = hist_symbol("anchor_h", "bbbb2222", T2);
    let (caller_b_id, c2) = hist_symbol("caller_b", "bbbb2222", T2);
    graph.push(a2);
    graph.push(c2);

    let hist_call = |from: &str, to: &str, commit: &str, vt: &str| {
        GraphRecord::edge(
            EdgeLabel::Calls,
            from.to_owned(),
            to.to_owned(),
            Some("1.0".to_owned()),
            "historical call".to_owned(),
        )
        .with_resolution(CallResolution::Resolved)
        .with_temporal(temporal(commit, &[], vt))
    };
    graph.push(hist_call(&caller_a_id, &anchor_id, "aaaa1111", T1));
    graph.push(hist_call(&caller_b_id, &anchor_id, "bbbb2222", T2));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, anchor_id, caller_a_id, caller_b_id)
}

#[test]
fn at_commit_returns_reachability_as_of_that_commit() {
    let (_t, path, anchor_id, caller_a_id, caller_b_id) = seed_history();

    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "aaaa1111",
    ]);
    assert_eq!(header["at_commit"], "aaaa1111");
    let ids = row_ids(&rows);
    assert!(
        ids.contains(&caller_a_id.as_str()),
        "caller_a existed at c1"
    );
    assert!(
        !ids.contains(&caller_b_id.as_str()),
        "caller_b did not exist at c1"
    );

    let (_, rows2) = run_query(&[
        "query",
        "transitive-callers",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "bbbb2222",
    ]);
    let ids2 = row_ids(&rows2);
    assert!(
        ids2.contains(&caller_b_id.as_str()),
        "caller_b exists at c2"
    );
    assert!(
        !ids2.contains(&caller_a_id.as_str()),
        "caller_a's call is gone at c2"
    );
}

#[test]
fn as_of_selects_most_recent_commit_at_or_before_instant() {
    let (_t, path, anchor_id, caller_a_id, caller_b_id) = seed_history();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callers",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--as-of",
        "2026-01-15T00:00:00Z",
    ]);
    assert_eq!(
        header["at_commit"], "aaaa1111",
        "mid-January resolves to c1"
    );
    let ids = row_ids(&rows);
    assert!(ids.contains(&caller_a_id.as_str()));
    assert!(!ids.contains(&caller_b_id.as_str()));
}

#[test]
fn at_missing_commit_exit2() {
    let (_t, path, anchor_id, _a, _b) = seed_history();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &anchor_id,
            "--graph",
            path.to_str().unwrap(),
            "--at",
            "ffffffff",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "missing_commit");
}

#[test]
fn invalid_as_of_timestamp_exit1() {
    let (_t, path, anchor_id, _a, _b) = seed_history();
    egregore()
        .args([
            "query",
            "transitive-callers",
            &anchor_id,
            "--graph",
            path.to_str().unwrap(),
            "--as-of",
            "not-a-timestamp",
        ])
        .assert()
        .code(1);
}

#[test]
fn at_on_history_free_graph_exit2() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callers",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
            "--at",
            "aaaa1111",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("json");
    assert_eq!(v["error"]["code"], "empty_history");
}

// ---------------------------------------------------------------------------
// AC1 — the same workflow answers from an ingested embedded store.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_store_returns_same_reachable_set() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    // The embedded sink rejects the fixture's intentionally dangling edge
    // (missing source node), so ingest a copy without it; the reachable set
    // is unaffected because that edge never produces a row.
    let dangling_marker = "c".repeat(64);
    let content = fs::read_to_string(&f.graph).expect("read fixture");
    let kept: Vec<&str> = content
        .lines()
        .filter(|l| !l.contains(&dangling_marker))
        .collect();
    let cleaned = format!("{}\n", kept.join("\n"));
    let cleaned_path = temp_db.path().join("cleaned.jsonl");
    fs::write(&cleaned_path, cleaned).expect("write cleaned fixture");

    egregore()
        .arg("ingest")
        .arg(&cleaned_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let (_, graph_rows) = run_query(&[
        "query",
        "transitive-callers",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let stdout = egregore()
        .args(["query", "transitive-callers", &f.anchor_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (_, store_rows) = parse_ndjson(&stdout);

    let mut graph_ids: Vec<&str> = row_ids(&graph_rows);
    let mut store_ids: Vec<&str> = row_ids(&store_rows);
    graph_ids.sort_unstable();
    store_ids.sort_unstable();
    assert_eq!(graph_ids, store_ids, "graph and store views must agree");
}
