//! End-to-end tests for `eg query transitive-callees <handle>` (issue #253):
//! bounded, deterministic transitive OUTBOUND reachability with dependency
//! paths — the mirror of `eg query transitive-callers` (#139), sharing its
//! BFS/path/truncation/cycle machinery but traversing outbound edges over the
//! wider #123 `deps` label set with an explicit `unresolved` category.
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

fn edge(graph: &mut Graph, label: EdgeLabel, from: &str, to: &str, summary: &str) {
    graph.push(GraphRecord::edge(
        label,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        summary.to_owned(),
    ));
}

fn calls(graph: &mut Graph, from: &str, to: &str, resolution: Option<CallResolution>) {
    let mut e = GraphRecord::edge(
        EdgeLabel::Calls,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        "call edge".to_owned(),
    );
    if let Some(r) = resolution {
        e = e.with_resolution(r);
    }
    graph.push(e);
}

// ---------------------------------------------------------------------------
// Fixture — >= 20 symbols, outbound CALLS chains of depth >= 3, one
// mutual-recursion cycle, one unrelated same-name pair, a symbol reachable
// only via a >= 2-hop path, an ambiguous edge two hops out, IMPLEMENTS /
// REFERENCES / IMPORTS outbound edges, and both unresolved-target classes.
// ---------------------------------------------------------------------------

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    anchor_id: String,
    dep1_id: String,
    dep2_id: String,
    dep3_id: String,
    amb_direct_id: String,
    amb_two_id: String,
    cycle_x_id: String,
    cycle_y_id: String,
    trait_sym_id: String,
    ref_target_id: String,
    import_id: String,
    unresolved_diag_id: String,
    missing_target_id: String,
    dup_a_id: String,
    dup_b_id: String,
    dup_a_callee_id: String,
    dup_b_callee_id: String,
    tombstoned_id: String,
}

#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("transitive_callees.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-tce"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-tce".to_owned()),
        "Repository repo-tce".to_owned(),
    ));

    for p in [
        "src/anchor.rs",
        "src/chain.rs",
        "src/cycle.rs",
        "src/dup_a.rs",
        "src/dup_b.rs",
        "src/refs.rs",
        "src/pad.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // Queried symbol.
    let anchor_id = symbol(&mut graph, "src/anchor.rs", "target_fn", (5, 40));

    // Depth-3 outbound CALLS chain: target_fn -> dep1 -> dep2 -> dep3.
    let dep1_id = symbol(&mut graph, "src/chain.rs", "dep1", (5, 12));
    let dep2_id = symbol(&mut graph, "src/chain.rs", "dep2", (14, 22));
    let dep3_id = symbol(&mut graph, "src/chain.rs", "dep3", (24, 32));
    calls(&mut graph, &anchor_id, &dep1_id, Some(CallResolution::Resolved));
    calls(&mut graph, &dep1_id, &dep2_id, Some(CallResolution::Resolved));
    calls(&mut graph, &dep2_id, &dep3_id, Some(CallResolution::Resolved));

    // Direct ambiguous outbound call.
    let amb_direct_id = symbol(&mut graph, "src/chain.rs", "amb_direct", (34, 40));
    calls(
        &mut graph,
        &anchor_id,
        &amb_direct_id,
        Some(CallResolution::Ambiguous),
    );

    // Ambiguous edge two hops out: target_fn -(resolved)-> dep1 -(ambiguous)-> amb_two.
    let amb_two_id = symbol(&mut graph, "src/chain.rs", "amb_two", (42, 50));
    calls(
        &mut graph,
        &dep1_id,
        &amb_two_id,
        Some(CallResolution::Ambiguous),
    );

    // Mutual-recursion cycle reachable from the anchor: target_fn -> cycle_x,
    // cycle_x <-> cycle_y.
    let cycle_x_id = symbol(&mut graph, "src/cycle.rs", "cycle_x", (5, 12));
    let cycle_y_id = symbol(&mut graph, "src/cycle.rs", "cycle_y", (14, 20));
    calls(
        &mut graph,
        &anchor_id,
        &cycle_x_id,
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

    // IMPLEMENTS / REFERENCES / IMPORTS outbound edges (the wider deps set).
    let trait_sym_id = symbol(&mut graph, "src/refs.rs", "TraitSym", (5, 12));
    edge(
        &mut graph,
        EdgeLabel::Implements,
        &anchor_id,
        &trait_sym_id,
        "target_fn implements TraitSym",
    );
    let ref_target_id = symbol(&mut graph, "src/refs.rs", "REF_TARGET", (14, 16));
    edge(
        &mut graph,
        EdgeLabel::References,
        &anchor_id,
        &ref_target_id,
        "target_fn references REF_TARGET",
    );
    let import_id = stable_id(&["node", "import", "repo-tce", "src/anchor.rs", "serde"]);
    graph.push(GraphRecord::syntax_node(
        import_id.clone(),
        NodeKind::Import,
        "src/anchor.rs".to_owned(),
        span(2, 2),
        "serde".to_owned(),
        "rust",
        "Rust import serde".to_owned(),
    ));
    edge(
        &mut graph,
        EdgeLabel::Imports,
        &anchor_id,
        &import_id,
        "target_fn imports serde",
    );

    // Unresolved outbound call to a Diagnostic marker (issue #152).
    let unresolved_diag_id = stable_id(&[
        "node",
        "diagnostic",
        "unresolved-call",
        "repo-tce",
        "src/anchor.rs",
        "external_call",
    ]);
    graph.push(GraphRecord::syntax_node(
        unresolved_diag_id.clone(),
        NodeKind::Diagnostic,
        "src/anchor.rs".to_owned(),
        span(18, 18),
        "external_call".to_owned(),
        "rust",
        "unresolved call target external_call (no in-repo definition)".to_owned(),
    ));
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Calls,
            anchor_id.clone(),
            unresolved_diag_id.clone(),
            None,
            "target_fn calls external_call (unresolved)".to_owned(),
        )
        .with_resolution(CallResolution::Unresolved),
    );
    // Dangling outbound edge: the target record does not exist in the graph.
    let missing_target_id = format!("codegraph:v{SCHEMA_VERSION}:{}", "d".repeat(64));
    calls(&mut graph, &anchor_id, &missing_target_id, None);

    // A self CALLS edge (recursion): the anchor is never its own callee.
    calls(&mut graph, &anchor_id, &anchor_id, Some(CallResolution::Resolved));

    // Unrelated same-name pair with their own outbound edges (zero-bleed).
    let dup_a_id = symbol(&mut graph, "src/dup_a.rs", "dup_name", (5, 12));
    let dup_b_id = symbol(&mut graph, "src/dup_b.rs", "dup_name", (5, 12));
    let dup_a_callee_id = symbol(&mut graph, "src/dup_a.rs", "dup_a_callee", (14, 20));
    let dup_b_callee_id = symbol(&mut graph, "src/dup_b.rs", "dup_b_callee", (14, 20));
    calls(
        &mut graph,
        &dup_a_id,
        &dup_a_callee_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &dup_b_id,
        &dup_b_callee_id,
        Some(CallResolution::Resolved),
    );

    // Padding symbols with no edges from the anchor (>= 20 symbols total).
    for (i, name) in ["pad1", "pad2", "pad3", "pad4", "pad5"].iter().enumerate() {
        symbol(&mut graph, "src/pad.rs", name, (5 + i * 10, 12 + i * 10));
    }

    // Tombstoned symbol for the stale_handle test.
    let tombstoned_id = symbol(&mut graph, "src/pad.rs", "deleted_fn", (80, 90));
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
        dep1_id,
        dep2_id,
        dep3_id,
        amb_direct_id,
        amb_two_id,
        cycle_x_id,
        cycle_y_id,
        trait_sym_id,
        ref_target_id,
        import_id,
        unresolved_diag_id,
        missing_target_id,
        dup_a_id,
        dup_b_id,
        dup_a_callee_id,
        dup_b_callee_id,
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

fn reachable_rows(rows: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    rows.iter()
        .filter(|r| r["category"].as_str() == Some("reachable"))
        .collect()
}

fn unresolved_rows(rows: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    rows.iter()
        .filter(|r| r["category"].as_str() == Some("unresolved"))
        .collect()
}

fn reachable_ids(rows: &[serde_json::Value]) -> Vec<&str> {
    reachable_rows(rows)
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect()
}

fn row_by_id<'a>(rows: &[&'a serde_json::Value], id: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["record_id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("row {id} missing"))
}

// ---------------------------------------------------------------------------
// AC1/AC2/AC3 — transitive reachable set with hop distances and concrete paths.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn reaches_transitive_callees_with_paths() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["direction"], "outbound");
    let labels: Vec<&str> = header["edge_labels"]
        .as_array()
        .expect("edge_labels array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(labels, ["CALLS", "IMPLEMENTS", "IMPORTS", "REFERENCES"]);
    assert_eq!(
        header["target"]["record_id"].as_str(),
        Some(f.anchor_id.as_str())
    );
    assert_eq!(header["max_depth"].as_u64(), Some(5), "safe default bound");
    let disclaimer = header["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("not"),
        "disclaimer must state leads-not-proof; got {disclaimer:?}"
    );

    let reach = reachable_rows(&rows);
    assert_eq!(
        usize::try_from(header["total_reachable"].as_u64().unwrap()).unwrap(),
        reach.len(),
        "header count must match emitted reachable rows"
    );

    let ids = reachable_ids(&rows);
    let expected = [
        &f.dep1_id,
        &f.dep2_id,
        &f.dep3_id,
        &f.amb_direct_id,
        &f.amb_two_id,
        &f.cycle_x_id,
        &f.cycle_y_id,
        &f.trait_sym_id,
        &f.ref_target_id,
        &f.import_id,
    ];
    for id in expected {
        assert!(ids.contains(&id.as_str()), "missing reachable row {id}");
    }
    assert_eq!(ids.len(), expected.len(), "no false positives: got {ids:?}");

    // Hop distances (shortest paths).
    assert_eq!(row_by_id(&reach, &f.dep1_id)["hop"], 1);
    assert_eq!(row_by_id(&reach, &f.cycle_x_id)["hop"], 1);
    assert_eq!(row_by_id(&reach, &f.trait_sym_id)["hop"], 1);
    assert_eq!(row_by_id(&reach, &f.import_id)["hop"], 1);
    assert_eq!(row_by_id(&reach, &f.dep2_id)["hop"], 2);
    assert_eq!(row_by_id(&reach, &f.amb_two_id)["hop"], 2);
    assert_eq!(row_by_id(&reach, &f.cycle_y_id)["hop"], 2);
    assert_eq!(row_by_id(&reach, &f.dep3_id)["hop"], 3);

    // Every reachable row carries the citable fields plus a concrete path that
    // runs from the anchor OUT to this node.
    for row in &reach {
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
        // First step starts at the anchor; last step ends at this node.
        assert_eq!(
            path.first().unwrap()["source_record_id"].as_str(),
            Some(f.anchor_id.as_str()),
            "first path step starts at the queried anchor"
        );
        assert_eq!(
            path.last().unwrap()["target_record_id"].as_str(),
            row["record_id"].as_str(),
            "last path step ends at the row's own record"
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

    // dep3's 3-hop path names the exact chain anchor -> dep1 -> dep2 -> dep3.
    let dep3_path = row_by_id(&reach, &f.dep3_id)["path"].as_array().unwrap();
    let chain: Vec<&str> = dep3_path
        .iter()
        .map(|s| s["source_record_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        chain,
        vec![f.anchor_id.as_str(), f.dep1_id.as_str(), f.dep2_id.as_str()]
    );
    assert_eq!(
        dep3_path.last().unwrap()["target_record_id"].as_str(),
        Some(f.dep3_id.as_str())
    );
}

// ---------------------------------------------------------------------------
// Resolution semantics (issues #152/#134) propagate weakest along paths.
// ---------------------------------------------------------------------------

#[test]
fn resolution_propagates_weakest_along_path() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let reach = reachable_rows(&rows);

    // Fully resolved chain.
    assert_eq!(row_by_id(&reach, &f.dep3_id)["path_resolution"], "resolved");
    assert_eq!(row_by_id(&reach, &f.dep1_id)["path_resolution"], "resolved");

    // Direct ambiguous edge.
    assert_eq!(
        row_by_id(&reach, &f.amb_direct_id)["path_resolution"],
        "ambiguous"
    );

    // Ambiguous edge two hops out weakens the whole path.
    let deep = row_by_id(&reach, &f.amb_two_id);
    assert_eq!(deep["path_resolution"], "ambiguous");
    let labels: Vec<Option<&str>> = deep["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["resolution"].as_str())
        .collect();
    assert_eq!(
        labels,
        vec![Some("resolved"), Some("ambiguous")],
        "per-step resolution must be exposed in anchor->out order"
    );

    // IMPLEMENTS / REFERENCES / IMPORTS edges carry no resolution contract:
    // path_resolution is absent for a pure non-CALLS chain.
    let ref_row = row_by_id(&reach, &f.ref_target_id);
    assert!(
        ref_row.get("path_resolution").is_none(),
        "a path with no labeled CALLS edge must omit path_resolution; got {ref_row}"
    );
    let import_row = row_by_id(&reach, &f.import_id);
    assert!(import_row.get("path_resolution").is_none());
}

// ---------------------------------------------------------------------------
// AC4 — explicit unresolved category: never dropped, never counted reachable.
// ---------------------------------------------------------------------------

#[test]
fn unresolved_outbound_targets_reported_as_explicit_category() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let unresolved = unresolved_rows(&rows);
    assert_eq!(
        usize::try_from(header["total_unresolved"].as_u64().unwrap()).unwrap(),
        unresolved.len(),
        "header unresolved count matches rows"
    );

    // Diagnostic-marker unresolved call.
    let diag_row = unresolved
        .iter()
        .find(|r| r["record_id"].as_str() == Some(f.unresolved_diag_id.as_str()))
        .unwrap_or_else(|| panic!("unresolved call row missing: {unresolved:?}"));
    assert_eq!(diag_row["category"], "unresolved");
    assert_eq!(diag_row["relation"], "CALLS");
    assert_eq!(diag_row["reason"], "unresolved_call");
    assert_eq!(diag_row["name"], "external_call");
    assert_eq!(diag_row["repo_relative_path"], "src/anchor.rs");
    assert_eq!(diag_row["resolution"], "unresolved");
    assert!(diag_row["edge_record_id"].as_str().is_some());
    assert_eq!(
        diag_row["source_record_id"].as_str(),
        Some(f.anchor_id.as_str()),
        "the unresolved edge departs from the anchor at hop 0"
    );
    assert_eq!(diag_row["source_hop"].as_u64(), Some(0));

    // Dangling edge with a missing target record.
    let dangling = unresolved
        .iter()
        .find(|r| r["target_record_id"].as_str() == Some(f.missing_target_id.as_str()))
        .unwrap_or_else(|| panic!("dangling-edge row missing: {unresolved:?}"));
    assert_eq!(dangling["reason"], "missing_target");
    assert_eq!(dangling["relation"], "CALLS");
    assert!(
        dangling.get("record_id").is_none(),
        "no live record to cite"
    );

    // Exactly the two hand-labeled unresolved rows.
    assert_eq!(unresolved.len(), 2, "got {unresolved:?}");

    // Never counted as reachable.
    let reach_ids = reachable_ids(&rows);
    assert!(
        !reach_ids.contains(&f.unresolved_diag_id.as_str()),
        "a Diagnostic target is never a reachable node"
    );
    assert!(!reach_ids.contains(&f.missing_target_id.as_str()));
}

// ---------------------------------------------------------------------------
// AC5 — the --max-depth=1 result exactly equals the `eg query deps` set.
// ---------------------------------------------------------------------------

#[test]
fn max_depth_1_equals_direct_deps_set() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
        "--max-depth",
        "1",
    ]);
    // The depth-1 handles: reachable record IDs + unresolved target record IDs.
    let mut got: Vec<String> = reachable_ids(&rows)
        .iter()
        .map(|s| (*s).to_owned())
        .chain(
            unresolved_rows(&rows)
                .iter()
                .filter_map(|r| r["target_record_id"].as_str().map(str::to_owned)),
        )
        .collect();
    got.sort_unstable();
    got.dedup();

    // `eg query deps` on the same anchor: dependency record IDs + unresolved
    // target record IDs.
    let stdout = egregore()
        .args([
            "query",
            "deps",
            &f.anchor_id,
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (_, deps_rows) = parse_ndjson(&stdout);
    let mut expected: Vec<String> = deps_rows
        .iter()
        .filter(|r| r["category"].as_str() == Some("dependency"))
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .chain(
            deps_rows
                .iter()
                .filter(|r| r["category"].as_str() == Some("unresolved"))
                .filter_map(|r| r["target_record_id"].as_str().map(str::to_owned)),
        )
        .collect();
    expected.sort_unstable();
    expected.dedup();

    assert_eq!(
        got, expected,
        "depth-1 reachable+unresolved set must match the deps set (no drift)"
    );
}

// ---------------------------------------------------------------------------
// AC4 — --max-depth bound, truncation diagnostic with dropped counts.
// ---------------------------------------------------------------------------

#[test]
fn max_depth_bounds_walk_and_reports_dropped_frontier() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
        "--max-depth",
        "2",
    ]);

    let ids = reachable_ids(&rows);
    assert!(!ids.contains(&f.dep3_id.as_str()), "dep3 is beyond depth 2");
    assert!(ids.contains(&f.dep2_id.as_str()), "dep2 is at depth 2");

    let trunc = &header["truncation"];
    assert!(trunc.is_object(), "truncation diagnostic required: {header}");
    assert_eq!(trunc["max_depth"].as_u64(), Some(2));
    let dropped = trunc["dropped_frontier"]
        .as_array()
        .expect("dropped_frontier");
    assert_eq!(dropped.len(), 1, "one dropped depth expected; got {dropped:?}");
    assert_eq!(dropped[0]["depth"].as_u64(), Some(3));
    assert_eq!(dropped[0]["count"].as_u64(), Some(1), "dep3 dropped at depth 3");
    assert_eq!(trunc["dropped_total"].as_u64(), Some(1));
}

#[test]
fn no_truncation_when_walk_exhausts_graph() {
    let f = seed();
    let (header, _) = run_query(&[
        "query",
        "transitive-callees",
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
fn max_depth_zero_rejected_exit1() {
    let f = seed();
    egregore()
        .args([
            "query",
            "transitive-callees",
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
                "transitive-callees",
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
    let (h_small, r_small) = parse_ndjson(&small);
    let (h_huge, r_huge) = parse_ndjson(&huge);
    assert_eq!(r_small, r_huge, "rows must match once the graph is exhausted");
    assert_eq!(h_small["total_reachable"], h_huge["total_reachable"]);
    assert_eq!(h_small["total_unresolved"], h_huge["total_unresolved"]);
}

// ---------------------------------------------------------------------------
// Cycle handling: reported once with the shortest discovered path, terminates.
// ---------------------------------------------------------------------------

#[test]
fn cycle_terminates_and_reports_each_symbol_once() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let reach = reachable_rows(&rows);

    let x_rows = reach
        .iter()
        .filter(|r| r["record_id"].as_str() == Some(f.cycle_x_id.as_str()))
        .count();
    let y_rows = reach
        .iter()
        .filter(|r| r["record_id"].as_str() == Some(f.cycle_y_id.as_str()))
        .count();
    assert_eq!(x_rows, 1, "cycle member reported exactly once");
    assert_eq!(y_rows, 1, "cycle member reported exactly once");

    assert_eq!(row_by_id(&reach, &f.cycle_x_id)["hop"], 1);
    assert_eq!(
        row_by_id(&reach, &f.cycle_y_id)["hop"],
        2,
        "cycle_y's shortest path is via cycle_x"
    );

    // The anchor is never resurfaced as its own callee (self-call excluded).
    assert!(
        !reachable_ids(&rows).contains(&f.anchor_id.as_str()),
        "anchor must not appear in its own reachable set"
    );
}

// ---------------------------------------------------------------------------
// Handle resolution & exit codes.
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_name_exit1_lists_all_candidates() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "transitive-callees",
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
    assert!(ids.contains(&f.dup_a_id.as_str()), "candidate A: {ids:?}");
    assert!(ids.contains(&f.dup_b_id.as_str()), "candidate B: {ids:?}");
}

#[test]
fn record_id_disambiguates_with_zero_same_name_bleed() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.dup_a_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let ids = reachable_ids(&rows);
    assert!(
        ids.contains(&f.dup_a_callee_id.as_str()),
        "dup_a's own callee must be reachable"
    );
    assert!(
        !ids.contains(&f.dup_b_callee_id.as_str()),
        "the unrelated same-name symbol's callee must never bleed in"
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
        "transitive-callees",
        "target_fn",
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
            "transitive-callees",
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
            "transitive-callees",
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
            "transitive-callees",
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
        .args(["query", "transitive-callees", "", "--graph"])
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
            "transitive-callees",
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
        "file handles are out of scope for transitive-callees; got {v}"
    );
}

#[test]
fn no_outbound_edges_is_explicit_empty_result_exit0() {
    let f = seed();
    let pad_id = sym_id("src/pad.rs", "pad1");
    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &pad_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["ok"], true);
    assert_eq!(header["total_reachable"].as_u64(), Some(0));
    assert_eq!(header["total_unresolved"].as_u64(), Some(0));
    assert!(rows.is_empty(), "no rows for a leaf symbol");
}

// ---------------------------------------------------------------------------
// Determinism, ordering, text, redaction, alias.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_output_x5() {
    let f = seed();
    let run = || -> Vec<u8> {
        egregore()
            .args([
                "query",
                "transitive-callees",
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

#[test]
fn rows_sorted_by_hop_then_record_id() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let keys: Vec<(u64, &str)> = reachable_rows(&rows)
        .iter()
        .map(|r| (r["hop"].as_u64().unwrap(), r["record_id"].as_str().unwrap()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "reachable rows must be canonically ordered");
}

#[test]
fn text_format_lists_reachable_symbols() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callees",
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
    assert!(out.contains("dep3"), "text mode must list reachable symbols: {out}");
    assert!(out.contains("hop"), "text mode must show hop distances: {out}");
    assert!(
        out.contains("external_call"),
        "text mode must report unresolved: {out}"
    );
}

#[test]
fn output_carries_handles_not_payloads() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callees",
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
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("json line");
        assert!(v.get("text").is_none(), "no raw text payloads: {line}");
        assert!(v.get("body").is_none(), "no raw body payloads: {line}");
    }
}

#[test]
fn eg_alias_works() {
    let f = seed();
    eg().args(["query", "transitive-callees", &f.anchor_id, "--graph"])
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

/// History fixture: at c1 `anchor_h` calls `dep_a`; at c2 that call is gone and
/// `anchor_h` calls `dep_b` instead.
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
    let (dep_a_id, d1) = hist_symbol("dep_a", "aaaa1111", T1);
    graph.push(a1);
    graph.push(d1);
    let (_, a2) = hist_symbol("anchor_h", "bbbb2222", T2);
    let (dep_b_id, d2) = hist_symbol("dep_b", "bbbb2222", T2);
    graph.push(a2);
    graph.push(d2);

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
    graph.push(hist_call(&anchor_id, &dep_a_id, "aaaa1111", T1));
    graph.push(hist_call(&anchor_id, &dep_b_id, "bbbb2222", T2));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, anchor_id, dep_a_id, dep_b_id)
}

#[test]
fn at_commit_returns_callees_as_of_that_commit() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history();

    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "aaaa1111",
    ]);
    assert_eq!(header["at_commit"], "aaaa1111");
    let ids = reachable_ids(&rows);
    assert!(ids.contains(&dep_a_id.as_str()), "dep_a existed at c1");
    assert!(!ids.contains(&dep_b_id.as_str()), "dep_b did not exist at c1");

    let (_, rows2) = run_query(&[
        "query",
        "transitive-callees",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "bbbb2222",
    ]);
    let ids2 = reachable_ids(&rows2);
    assert!(ids2.contains(&dep_b_id.as_str()), "dep_b exists at c2");
    assert!(!ids2.contains(&dep_a_id.as_str()), "dep_a's call is gone at c2");
}

#[test]
fn as_of_selects_most_recent_commit_at_or_before_instant() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history();
    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--as-of",
        "2026-01-15T00:00:00Z",
    ]);
    assert_eq!(header["at_commit"], "aaaa1111", "mid-January resolves to c1");
    let ids = reachable_ids(&rows);
    assert!(ids.contains(&dep_a_id.as_str()));
    assert!(!ids.contains(&dep_b_id.as_str()));
}

#[test]
fn repo_scoped_as_of_resolves_within_selected_repository() {
    // Two repositories in one shared store: repo A's only commit (aaaa1111 @
    // T1) is older than repo B's (bbbb2222 @ T2). Scoped to repo A, --as-of
    // must resolve the temporal view within repo A.
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("multi-repo-history.jsonl");
    let mut graph = Graph::new();

    let repo_a_id = stable_id(&["node", "Repository", "repo-tea"]);
    let repo_b_id = stable_id(&["node", "Repository", "repo-teb"]);
    for (id, tag) in [(&repo_a_id, "repo-tea"), (&repo_b_id, "repo-teb")] {
        graph.push(GraphRecord::node(
            id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(tag.to_owned()),
            format!("Repository {tag}"),
        ));
    }
    let mut commit_in = |repo_id: &str, repo_tag: &str, sha: &str, vt: &str| {
        let commit_id = stable_id(&["node", "commit", repo_tag, sha]);
        graph.push(
            GraphRecord::node(
                commit_id.clone(),
                NodeKind::Commit,
                None,
                None,
                Some(sha.to_owned()),
                format!("Commit {sha} in {repo_tag}"),
            )
            .with_temporal(temporal(sha, &[], vt)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.to_owned(),
            commit_id,
            None,
            format!("{repo_tag} contains commit {sha}"),
        ));
    };
    commit_in(&repo_a_id, "repo-tea", "aaaa1111", T1);
    commit_in(&repo_b_id, "repo-teb", "bbbb2222", T2);

    file(&mut graph, &repo_a_id, "src/ta.rs");
    let hist_symbol = |graph: &mut Graph, name: &str, sha: &str, vt: &str| -> String {
        let id = sym_id("src/ta.rs", name);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Symbol,
                "src/ta.rs".to_owned(),
                span(1, 10),
                name.to_owned(),
                "rust",
                format!("fn {name}"),
            )
            .with_temporal(temporal(sha, &[], vt)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id("src/ta.rs"),
            id.clone(),
            None,
            format!("src/ta.rs defines {name}"),
        ));
        id
    };
    let anchor_id = hist_symbol(&mut graph, "anchor_ta", "aaaa1111", T1);
    let callee_id = hist_symbol(&mut graph, "callee_ta", "aaaa1111", T1);
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Calls,
            anchor_id.clone(),
            callee_id.clone(),
            Some("1.0".to_owned()),
            "anchor_ta calls callee_ta".to_owned(),
        )
        .with_resolution(CallResolution::Resolved)
        .with_temporal(temporal("aaaa1111", &[], T1)),
    );
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (header, rows) = run_query(&[
        "query",
        "transitive-callees",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--repo",
        "repo-tea",
        "--as-of",
        "2026-03-01T00:00:00Z",
    ]);
    assert_eq!(
        header["at_commit"], "aaaa1111",
        "--as-of must resolve within the selected repository"
    );
    let ids = reachable_ids(&rows);
    assert!(ids.contains(&callee_id.as_str()), "callee_ta answered at c1");
}

#[test]
fn at_missing_commit_exit2() {
    let (_t, path, anchor_id, _a, _b) = seed_history();
    let stdout = egregore()
        .args([
            "query",
            "transitive-callees",
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
            "transitive-callees",
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
            "transitive-callees",
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
// The same workflow answers from an ingested embedded store.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_store_returns_same_reachable_set() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    // The embedded sink rejects the fixture's intentionally dangling edge
    // (missing target node), so ingest a copy without it; the reachable set is
    // unaffected because that edge only produces an unresolved row.
    let dangling_marker = "d".repeat(64);
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
        "transitive-callees",
        &f.anchor_id,
        "--graph",
        cleaned_path.to_str().unwrap(),
    ]);
    let stdout = egregore()
        .args(["query", "transitive-callees", &f.anchor_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (_, store_rows) = parse_ndjson(&stdout);

    let mut graph_ids: Vec<&str> = reachable_ids(&graph_rows);
    let mut store_ids: Vec<&str> = reachable_ids(&store_rows);
    graph_ids.sort_unstable();
    store_ids.sort_unstable();
    assert_eq!(graph_ids, store_ids, "graph and store views must agree");
}
