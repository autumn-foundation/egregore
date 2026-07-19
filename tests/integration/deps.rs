//! End-to-end tests for `eg query deps <handle>` (issue #123): deterministic
//! direct outbound dependencies of a symbol — what it calls, implements,
//! references, and imports — with an explicit `unresolved` category.
#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload,
    SourceSpan, TemporalMetadata,
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
// Fixture — a hand-labeled dependency graph for one anchor symbol. The
// expected direct outbound set is known exactly (AC7): CALLS (resolved,
// ambiguous fan-out, legacy no-resolution), IMPLEMENTS, REFERENCES, IMPORTS,
// one unresolved call targeting a Diagnostic marker, one dangling edge whose
// target record is missing, plus inbound / self / weak-label / tombstoned
// noise that must never appear.
// ---------------------------------------------------------------------------

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    anchor_id: String,
    callee_resolved_id: String,
    callee_legacy_id: String,
    amb_one_id: String,
    amb_two_id: String,
    trait_sym_id: String,
    ref_target_id: String,
    import_id: String,
    unresolved_diag_id: String,
    missing_target_id: String,
    ghost_id: String,
    caller_fn_id: String,
    mentioned_id: String,
    dup_a_id: String,
    dup_b_id: String,
    tombstoned_id: String,
}

#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("deps.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-deps"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-deps".to_owned()),
        "Repository repo-deps".to_owned(),
    ));

    for p in [
        "src/anchor.rs",
        "src/deps.rs",
        "src/other.rs",
        "src/dup_a.rs",
        "src/dup_b.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // The queried symbol.
    let anchor_id = symbol(&mut graph, "src/anchor.rs", "target_fn", (5, 40));

    // ── expected dependencies (hand-labeled ground truth) ─────────────────────
    // CALLS, resolved.
    let callee_resolved_id = symbol(&mut graph, "src/deps.rs", "callee_resolved", (5, 12));
    calls(
        &mut graph,
        &anchor_id,
        &callee_resolved_id,
        Some(CallResolution::Resolved),
    );
    // CALLS, legacy edge without a resolution field.
    let callee_legacy_id = symbol(&mut graph, "src/deps.rs", "callee_legacy", (14, 20));
    calls(&mut graph, &anchor_id, &callee_legacy_id, None);
    // CALLS, ambiguous fan-out to two candidates.
    let amb_one_id = symbol(&mut graph, "src/dup_a.rs", "amb_candidate_one", (5, 12));
    let amb_two_id = symbol(&mut graph, "src/dup_b.rs", "amb_candidate_two", (5, 12));
    calls(
        &mut graph,
        &anchor_id,
        &amb_one_id,
        Some(CallResolution::Ambiguous),
    );
    calls(
        &mut graph,
        &anchor_id,
        &amb_two_id,
        Some(CallResolution::Ambiguous),
    );
    // IMPLEMENTS.
    let trait_sym_id = symbol(&mut graph, "src/deps.rs", "TraitSym", (22, 30));
    edge(
        &mut graph,
        EdgeLabel::Implements,
        &anchor_id,
        &trait_sym_id,
        "target_fn implements TraitSym",
    );
    // REFERENCES.
    let ref_target_id = symbol(&mut graph, "src/deps.rs", "REF_TARGET", (32, 34));
    edge(
        &mut graph,
        EdgeLabel::References,
        &anchor_id,
        &ref_target_id,
        "target_fn references REF_TARGET",
    );
    // IMPORTS — an Import node dependency.
    let import_id = stable_id(&["node", "import", "repo-deps", "src/anchor.rs", "serde"]);
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

    // ── expected unresolved category ──────────────────────────────────────────
    // Unresolved call: the edge targets a Diagnostic marker node (issue #152).
    let unresolved_diag_id = stable_id(&[
        "node",
        "diagnostic",
        "unresolved-call",
        "repo-deps",
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
            "target_fn calls external_call (cross-file, unresolved)".to_owned(),
        )
        .with_resolution(CallResolution::Unresolved),
    );
    // Dangling edge: the target record does not exist in the graph at all.
    let missing_target_id = format!("codegraph:v{SCHEMA_VERSION}:{}", "d".repeat(64));
    calls(&mut graph, &anchor_id, &missing_target_id, None);
    // Live edge to a tombstoned (deleted) symbol: not in-graph, so unresolved.
    let ghost_id = symbol(&mut graph, "src/deps.rs", "ghost_fn", (36, 40));
    calls(
        &mut graph,
        &anchor_id,
        &ghost_id,
        Some(CallResolution::Resolved),
    );
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &ghost_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: ghost_id.clone(),
        summary: "ghost_fn was deleted".to_owned(),
        producer: None,
    });

    // ── noise that must never appear ──────────────────────────────────────────
    // Inbound edges: someone calling/referencing the anchor.
    let caller_fn_id = symbol(&mut graph, "src/other.rs", "caller_fn", (5, 12));
    calls(
        &mut graph,
        &caller_fn_id,
        &anchor_id,
        Some(CallResolution::Resolved),
    );
    edge(
        &mut graph,
        EdgeLabel::References,
        &caller_fn_id,
        &anchor_id,
        "caller_fn references target_fn",
    );
    // Unrelated symbol with its own outbound edge.
    let other_fn_id = symbol(&mut graph, "src/other.rs", "other_fn", (14, 20));
    calls(
        &mut graph,
        &other_fn_id,
        &callee_resolved_id,
        Some(CallResolution::Resolved),
    );
    // A weak MENTIONS edge from the anchor (excluded label).
    let mentioned_id = symbol(&mut graph, "src/other.rs", "mentioned_sym", (22, 30));
    edge(
        &mut graph,
        EdgeLabel::Mentions,
        &anchor_id,
        &mentioned_id,
        "target_fn mentions mentioned_sym",
    );
    // A self CALLS edge (recursion) — the anchor is never its own dependency.
    calls(
        &mut graph,
        &anchor_id,
        &anchor_id,
        Some(CallResolution::Resolved),
    );
    // A tombstoned (deleted) outbound edge: the dependency was retired.
    let retired_id = symbol(&mut graph, "src/deps.rs", "retired_fn", (42, 48));
    let retired_edge = GraphRecord::edge(
        EdgeLabel::Calls,
        anchor_id.clone(),
        retired_id.clone(),
        Some("1.0".to_owned()),
        "target_fn calls retired_fn".to_owned(),
    )
    .with_resolution(CallResolution::Resolved);
    let retired_edge_id = retired_edge.id().to_owned();
    graph.push(retired_edge);
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &retired_edge_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: retired_edge_id,
        summary: "call to retired_fn was deleted".to_owned(),
        producer: None,
    });
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &retired_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: retired_id,
        summary: "retired_fn was deleted".to_owned(),
        producer: None,
    });

    // A leaf symbol with no outbound edges (explicit empty result).
    symbol(&mut graph, "src/other.rs", "leaf_fn", (32, 40));

    // Unrelated same-name pair (ambiguous-name resolution).
    let dup_a_id = symbol(&mut graph, "src/dup_a.rs", "dup_name", (14, 20));
    let dup_b_id = symbol(&mut graph, "src/dup_b.rs", "dup_name", (14, 20));
    calls(
        &mut graph,
        &dup_a_id,
        &callee_resolved_id,
        Some(CallResolution::Resolved),
    );
    calls(
        &mut graph,
        &dup_b_id,
        &callee_legacy_id,
        Some(CallResolution::Resolved),
    );

    // Tombstoned symbol for the stale_handle test.
    let tombstoned_id = symbol(&mut graph, "src/other.rs", "deleted_fn", (42, 50));
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
        callee_resolved_id,
        callee_legacy_id,
        amb_one_id,
        amb_two_id,
        trait_sym_id,
        ref_target_id,
        import_id,
        unresolved_diag_id,
        missing_target_id,
        ghost_id,
        caller_fn_id,
        mentioned_id,
        dup_a_id,
        dup_b_id,
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

fn dependency_rows(rows: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    rows.iter()
        .filter(|r| r["category"].as_str() == Some("dependency"))
        .collect()
}

fn unresolved_rows(rows: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    rows.iter()
        .filter(|r| r["category"].as_str() == Some("unresolved"))
        .collect()
}

fn row_by_id<'a>(rows: &[&'a serde_json::Value], id: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["record_id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("row {id} missing"))
}

// ---------------------------------------------------------------------------
// AC1/AC2/AC3/AC7 — exact direct outbound dependency set: zero false
// positives, zero false negatives, edge-type labels, citable handles.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn returns_exact_outbound_dependency_set() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["direction"], "outbound");
    assert_eq!(
        header["target"]["record_id"].as_str(),
        Some(f.anchor_id.as_str())
    );
    let labels: Vec<&str> = header["edge_labels"]
        .as_array()
        .expect("edge_labels array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(labels, ["CALLS", "IMPLEMENTS", "IMPORTS", "REFERENCES"]);
    let disclaimer = header["disclaimer"].as_str().expect("disclaimer");
    assert!(
        disclaimer.contains("not proof"),
        "disclaimer must state leads-not-proof; got {disclaimer:?}"
    );

    // ── exact dependency set (zero FP / zero FN) ──────────────────────────────
    let deps = dependency_rows(&rows);
    let dep_ids: Vec<&str> = deps
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    let expected = [
        &f.callee_resolved_id,
        &f.callee_legacy_id,
        &f.amb_one_id,
        &f.amb_two_id,
        &f.trait_sym_id,
        &f.ref_target_id,
        &f.import_id,
    ];
    for id in expected {
        assert!(dep_ids.contains(&id.as_str()), "missing dependency {id}");
    }
    assert_eq!(
        dep_ids.len(),
        expected.len(),
        "no false positives: got {dep_ids:?}"
    );

    // Edge-type labels (AC3).
    assert_eq!(row_by_id(&deps, &f.callee_resolved_id)["relation"], "CALLS");
    assert_eq!(row_by_id(&deps, &f.callee_legacy_id)["relation"], "CALLS");
    assert_eq!(row_by_id(&deps, &f.trait_sym_id)["relation"], "IMPLEMENTS");
    assert_eq!(row_by_id(&deps, &f.ref_target_id)["relation"], "REFERENCES");
    assert_eq!(row_by_id(&deps, &f.import_id)["relation"], "IMPORTS");

    // Citable handles on every dependency row (AC2).
    for row in &deps {
        assert!(row["record_id"].as_str().is_some(), "record_id required");
        assert!(row["schema_version"].is_number(), "schema_version required");
        assert!(row["name"].as_str().is_some(), "name required");
        assert!(row["kind"].as_str().is_some(), "kind required");
        assert!(
            row["repo_relative_path"].as_str().is_some(),
            "repo_relative_path required"
        );
        assert!(row["span"].is_object(), "span required");
        assert!(
            row["edge_record_id"].as_str().is_some(),
            "the producing edge must be citable"
        );
    }

    // Import rows keep their node kind so the reader knows this is an import
    // declaration, not a resolved symbol definition.
    assert_eq!(row_by_id(&deps, &f.import_id)["kind"], "Import");

    // Resolution labels propagate (issues #152/#134): resolved / ambiguous
    // carried, legacy edges omit the field rather than inventing a status.
    assert_eq!(
        row_by_id(&deps, &f.callee_resolved_id)["resolution"],
        "resolved"
    );
    assert_eq!(row_by_id(&deps, &f.amb_one_id)["resolution"], "ambiguous");
    assert_eq!(row_by_id(&deps, &f.amb_two_id)["resolution"], "ambiguous");
    assert!(
        row_by_id(&deps, &f.callee_legacy_id)
            .get("resolution")
            .is_none(),
        "legacy CALLS edge without a resolution field must not invent one"
    );

    // Header counts match the emitted rows.
    assert_eq!(
        usize::try_from(header["total_dependencies"].as_u64().unwrap()).unwrap(),
        deps.len()
    );
    assert_eq!(
        usize::try_from(header["total_unresolved"].as_u64().unwrap()).unwrap(),
        unresolved_rows(&rows).len()
    );
}

// ---------------------------------------------------------------------------
// AC3 — unresolved targets are an explicit category, never silently dropped.
// ---------------------------------------------------------------------------

#[test]
fn unresolved_call_reported_as_explicit_category() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let unresolved = unresolved_rows(&rows);

    // The Diagnostic-marker unresolved call (issue #152).
    let diag_row = unresolved
        .iter()
        .find(|r| r["record_id"].as_str() == Some(f.unresolved_diag_id.as_str()))
        .unwrap_or_else(|| panic!("unresolved call row missing: {unresolved:?}"));
    assert_eq!(diag_row["category"], "unresolved");
    assert_eq!(diag_row["relation"], "CALLS");
    assert_eq!(diag_row["reason"], "unresolved_call");
    assert_eq!(
        diag_row["name"], "external_call",
        "the callee display name must be reported"
    );
    assert_eq!(diag_row["repo_relative_path"], "src/anchor.rs");
    assert!(diag_row["span"].is_object(), "call-site span is citable");
    assert_eq!(diag_row["resolution"], "unresolved");
    assert!(diag_row["edge_record_id"].as_str().is_some());

    // The Diagnostic marker never appears as a normal dependency.
    assert!(
        !dependency_rows(&rows)
            .iter()
            .any(|r| r["record_id"].as_str() == Some(f.unresolved_diag_id.as_str())),
        "a Diagnostic target is not a resolved dependency"
    );
}

#[test]
fn missing_target_reported_as_unresolved() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let unresolved = unresolved_rows(&rows);

    // Dangling edge — the target record does not exist in the graph.
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

    // Live edge to a tombstoned (deleted) symbol is likewise not in-graph.
    let ghost = unresolved
        .iter()
        .find(|r| r["target_record_id"].as_str() == Some(f.ghost_id.as_str()))
        .unwrap_or_else(|| panic!("tombstoned-target row missing: {unresolved:?}"));
    assert_eq!(ghost["reason"], "missing_target");

    // Exactly the three hand-labeled unresolved rows: diagnostic, dangling,
    // tombstoned target (zero FP / zero FN in the unresolved category too).
    assert_eq!(unresolved.len(), 3, "got {unresolved:?}");
}

#[test]
fn unresolved_resolution_edge_with_absent_marker_stays_unresolved_call() {
    // A CALLS edge carrying `resolution: "unresolved"` whose Diagnostic marker
    // record is absent from the graph: the edge itself already identifies an
    // unresolved call, so the reason must stay `unresolved_call`;
    // `missing_target` is reserved for dangling edges without that signal.
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("absent-marker.jsonl");
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "repo-absent"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-absent".to_owned()),
        "Repository repo-absent".to_owned(),
    ));
    file(&mut graph, &repo_id, "src/a.rs");
    let anchor_id = symbol(&mut graph, "src/a.rs", "anchor_absent", (5, 10));
    let absent_marker_id = format!("codegraph:v{SCHEMA_VERSION}:{}", "e".repeat(64));
    calls(
        &mut graph,
        &anchor_id,
        &absent_marker_id,
        Some(CallResolution::Unresolved),
    );
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (_, rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
    ]);
    let unresolved = unresolved_rows(&rows);
    assert_eq!(unresolved.len(), 1, "got {unresolved:?}");
    assert_eq!(
        unresolved[0]["reason"], "unresolved_call",
        "the edge's unresolved signal outranks the absent marker record"
    );
    assert_eq!(unresolved[0]["resolution"], "unresolved");
    assert_eq!(unresolved[0]["target_record_id"], absent_marker_id);
    assert!(
        unresolved[0].get("record_id").is_none(),
        "no live marker record to cite"
    );
}

// ---------------------------------------------------------------------------
// Direction and label discipline — inbound edges, weak labels, self edges,
// and tombstoned edges never produce rows.
// ---------------------------------------------------------------------------

#[test]
fn inbound_weak_self_and_tombstoned_edges_never_reported() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    let all_ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        !all_ids.contains(&f.caller_fn_id.as_str()),
        "inbound callers are the other direction (issue #122), never a dependency"
    );
    assert!(
        !all_ids.contains(&f.mentioned_id.as_str()),
        "weak MENTIONS edges are outside the dependency contract"
    );
    assert!(
        !all_ids.contains(&f.anchor_id.as_str()),
        "the anchor is never its own dependency"
    );
    let retired_id = sym_id("src/deps.rs", "retired_fn");
    let names_retired_target = rows
        .iter()
        .filter_map(|r| r["target_record_id"].as_str())
        .any(|t| t == retired_id);
    assert!(
        !all_ids.contains(&retired_id.as_str()) && !names_retired_target,
        "a tombstoned edge produces no row at all"
    );
}

// ---------------------------------------------------------------------------
// AC5 — handle resolution and exit codes: 0 match, 1 ambiguous/malformed,
// 2 no match.
// ---------------------------------------------------------------------------

#[test]
fn unique_name_resolves_like_symbol_query() {
    let f = seed();
    let (header, rows) = run_query(&[
        "query",
        "deps",
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
fn ambiguous_name_exit1_lists_all_candidates() {
    let f = seed();
    let stderr = egregore()
        .args([
            "query",
            "deps",
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
fn unknown_handle_no_match_exit2() {
    let f = seed();
    let absent = format!("codegraph:v{}:{}", SCHEMA_VERSION, "b".repeat(64));
    let stdout = egregore()
        .args([
            "query",
            "deps",
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
            "deps",
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
            "deps",
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
        .args(["query", "deps", "", "--graph"])
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
            "deps",
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
        "file handles are out of scope for deps; got {v}"
    );
}

#[test]
fn no_outbound_deps_is_explicit_empty_result_exit0() {
    let f = seed();
    let leaf_id = sym_id("src/other.rs", "leaf_fn");
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &leaf_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["ok"], true);
    assert_eq!(header["total_dependencies"].as_u64(), Some(0));
    assert_eq!(header["total_unresolved"].as_u64(), Some(0));
    assert!(rows.is_empty(), "no rows for a leaf symbol");
}

// ---------------------------------------------------------------------------
// AC6 — deterministic, byte-identical NDJSON with canonical ordering.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_output_x5() {
    let f = seed();
    let run = || -> Vec<u8> {
        egregore()
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
            .clone()
    };
    let first = run();
    for i in 2..=5 {
        assert_eq!(first, run(), "run {i} not byte-identical");
    }
}

#[test]
fn rows_ordered_dependencies_then_unresolved_canonically() {
    let f = seed();
    let (_, rows) = run_query(&[
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    // All dependency rows precede all unresolved rows.
    let categories: Vec<&str> = rows.iter().filter_map(|r| r["category"].as_str()).collect();
    let first_unresolved = categories.iter().position(|c| *c == "unresolved");
    if let Some(pos) = first_unresolved {
        assert!(
            categories[pos..].iter().all(|c| *c == "unresolved"),
            "unresolved rows must come after all dependency rows: {categories:?}"
        );
    }
    // Dependency rows sorted by (relation, record_id, edge_record_id).
    let dep_keys: Vec<(String, String, String)> = dependency_rows(&rows)
        .iter()
        .map(|r| {
            (
                r["relation"].as_str().unwrap().to_owned(),
                r["record_id"].as_str().unwrap().to_owned(),
                r["edge_record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut sorted = dep_keys.clone();
    sorted.sort();
    assert_eq!(
        dep_keys, sorted,
        "dependency rows must be canonically sorted"
    );
    // Unresolved rows sorted by (relation, target_record_id, edge_record_id).
    let unres_keys: Vec<(String, String, String)> = unresolved_rows(&rows)
        .iter()
        .map(|r| {
            (
                r["relation"].as_str().unwrap().to_owned(),
                r["target_record_id"].as_str().unwrap().to_owned(),
                r["edge_record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut unres_sorted = unres_keys.clone();
    unres_sorted.sort();
    assert_eq!(
        unres_keys, unres_sorted,
        "unresolved rows must be canonically sorted"
    );
}

// ---------------------------------------------------------------------------
// AC6 — --format text mode.
// ---------------------------------------------------------------------------

#[test]
fn text_format_lists_dependencies() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "deps",
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
    assert!(out.contains("target_fn"), "names the queried symbol: {out}");
    assert!(out.contains("callee_resolved"), "lists a callee: {out}");
    assert!(out.contains("CALLS"), "labels the edge type: {out}");
    assert!(out.contains("external_call"), "reports unresolved: {out}");
}

#[test]
fn eg_alias_works() {
    let f = seed();
    eg().args([
        "query",
        "deps",
        &f.anchor_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ])
    .assert()
    .success();
}

// ---------------------------------------------------------------------------
// Output carries handles, never payloads.
// ---------------------------------------------------------------------------

#[test]
fn output_carries_handles_not_payloads() {
    let f = seed();
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
    let out = String::from_utf8(stdout).expect("utf8");
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("json line");
        assert!(v.get("text").is_none(), "no raw text payloads: {line}");
        assert!(v.get("body").is_none(), "no raw body payloads: {line}");
    }
}

// ---------------------------------------------------------------------------
// AC4 — temporal selectors --at / --as-of over a history graph.
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

/// History fixture: at c1 `anchor_h` calls `dep_a`; at c2 that call is gone
/// and `anchor_h` calls `dep_b` instead.
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
fn at_commit_returns_dependencies_as_of_that_commit() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history();

    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "aaaa1111",
    ]);
    assert_eq!(header["at_commit"], "aaaa1111");
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(&dep_a_id.as_str()), "dep_a existed at c1");
    assert!(
        !ids.contains(&dep_b_id.as_str()),
        "dep_b did not exist at c1"
    );

    let (_, rows2) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at",
        "bbbb2222",
    ]);
    let ids2: Vec<&str> = rows2
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids2.contains(&dep_b_id.as_str()), "dep_b exists at c2");
    assert!(
        !ids2.contains(&dep_a_id.as_str()),
        "dep_a's call is gone at c2"
    );
}

#[test]
fn as_of_selects_most_recent_commit_at_or_before_instant() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history();
    let (header, rows) = run_query(&[
        "query",
        "deps",
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
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(&dep_a_id.as_str()));
    assert!(!ids.contains(&dep_b_id.as_str()));
}

// ---------------------------------------------------------------------------
// Issue #427 — corpus-mode default (HEAD-anchored) vs opt-in union.
// ---------------------------------------------------------------------------

/// History fixture WITH a `source_snapshot` HEAD at c2 (`bbbb2222`).
///
/// At c1 `anchor_h` calls `dep_a`; at c2 that call is gone and `anchor_h` calls
/// `dep_b` instead. The `Repository` node carries a `source_snapshot` pinning
/// HEAD to c2, so head-anchoring is possible: the deleted-at-HEAD `dep_a`
/// dependency must be excluded from the default/`--at-head` corpus but present
/// in the `--all-history` union. Returns `(temp, path, anchor_id, dep_a_id,
/// dep_b_id)`.
fn seed_history_snapshot() -> (tempfile::TempDir, PathBuf, String, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history-snapshot.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-h427"]);
    graph.push(
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("repo-h427".to_owned()),
            "Repository repo-h427".to_owned(),
        )
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit {
                sha: "bbbb2222".to_owned(),
            },
            dirty: false,
            repository_id: repo_id.clone(),
            scanned_at: T2.to_owned(),
        }),
    );

    // Commits, attributed to the repo via CONTAINS.
    let mut commit_in = |sha: &str, parents: &[&str], vt: &str| {
        let commit_id = stable_id(&["node", "commit", "repo-h427", sha]);
        graph.push(
            GraphRecord::node(
                commit_id.clone(),
                NodeKind::Commit,
                None,
                None,
                Some(sha.to_owned()),
                format!("Commit {sha}"),
            )
            .with_temporal(temporal(sha, parents, vt)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            commit_id,
            None,
            format!("repo contains commit {sha}"),
        ));
    };
    commit_in("aaaa1111", &[], T1);
    commit_in("bbbb2222", &["aaaa1111"], T2);

    // File attributed to the repo; symbols attributed to the file via DEFINES,
    // so `RepositoryIndex::owner_of` resolves each symbol to `repo-h427`.
    file(&mut graph, &repo_id, "src/h.rs");

    // Symbol + its DEFINES edge both carry temporal for the commit, mirroring
    // real `scan-history` so the embedded sink accepts edges to a multi-version
    // target node (the anchor exists at both c1 and c2).
    let hist_symbol = |graph: &mut Graph, name: &str, commit: &str, vt: &str| -> String {
        let id = sym_id("src/h.rs", name);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Symbol,
                "src/h.rs".to_owned(),
                span(1, 10),
                name.to_owned(),
                "rust",
                format!("fn {name}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id("src/h.rs"),
                id.clone(),
                None,
                format!("src/h.rs defines {name}"),
            )
            .with_temporal(temporal(commit, &[], vt)),
        );
        id
    };

    let anchor_id = hist_symbol(&mut graph, "anchor_h", "aaaa1111", T1);
    let dep_a_id = hist_symbol(&mut graph, "dep_a", "aaaa1111", T1);
    hist_symbol(&mut graph, "anchor_h", "bbbb2222", T2);
    let dep_b_id = hist_symbol(&mut graph, "dep_b", "bbbb2222", T2);

    let hist_call = |graph: &mut Graph, from: &str, to: &str, commit: &str, vt: &str| {
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
    hist_call(&mut graph, &anchor_id, &dep_a_id, "aaaa1111", T1);
    hist_call(&mut graph, &anchor_id, &dep_b_id, "bbbb2222", T2);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, anchor_id, dep_a_id, dep_b_id)
}

#[test]
fn all_history_flag_includes_deleted_at_head_dependency() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history_snapshot();
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--all-history",
    ]);
    assert_eq!(header["corpus_mode"], "union");
    assert_eq!(header["corpus_mode_source"], "explicit_flag");
    let ids: Vec<&str> = dependency_rows(&rows)
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        ids.contains(&dep_a_id.as_str()),
        "--all-history keeps the c1 dependency: {ids:?}"
    );
    assert!(
        ids.contains(&dep_b_id.as_str()),
        "--all-history keeps the c2 dependency: {ids:?}"
    );
}

#[test]
fn default_head_anchors_and_excludes_deleted_at_head_dependency() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history_snapshot();
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        header["corpus_mode"], "head_anchored",
        "a snapshot store defaults to head-anchored"
    );
    assert_eq!(header["corpus_mode_source"], "default");
    let ids: Vec<&str> = dependency_rows(&rows)
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(
        !ids.contains(&dep_a_id.as_str()),
        "the deleted-at-HEAD dep_a must not appear under the default: {ids:?}"
    );
    assert!(
        ids.contains(&dep_b_id.as_str()),
        "the HEAD dependency dep_b must appear: {ids:?}"
    );
    // dep_a must not resurface disguised as an unresolved row either.
    let unresolved: Vec<&str> = unresolved_rows(&rows)
        .iter()
        .filter_map(|r| r["target_record_id"].as_str())
        .collect();
    assert!(
        !unresolved.contains(&dep_a_id.as_str()),
        "head-anchoring drops the stale edge whole, never as unresolved: {unresolved:?}"
    );
}

#[test]
fn at_head_flag_matches_default_and_is_explicit() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history_snapshot();
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
        "--at-head",
    ]);
    assert_eq!(header["corpus_mode"], "head_anchored");
    assert_eq!(
        header["corpus_mode_source"], "explicit_flag",
        "--at-head records an explicit selection"
    );
    let ids: Vec<&str> = dependency_rows(&rows)
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(!ids.contains(&dep_a_id.as_str()), "{ids:?}");
    assert!(ids.contains(&dep_b_id.as_str()), "{ids:?}");
}

/// The conflicting corpus/temporal flags exit 1 with a machine-readable
/// `unsupported_combination` diagnostic on stdout.
fn assert_unsupported_combination(extra: &[&str]) {
    let (_t, path, anchor_id, _a, _b) = seed_history_snapshot();
    let mut args: Vec<String> = vec![
        "query".into(),
        "deps".into(),
        anchor_id,
        "--graph".into(),
        path.to_str().unwrap().to_owned(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let assert = egregore().args(&args).assert().failure().code(1);
    let stdout = assert.get_output().stdout.clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let value: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("envelope line")).expect("json envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(
        value["error"]["code"], "unsupported_combination",
        "expected unsupported_combination for {extra:?}: {out}"
    );
}

#[test]
fn at_head_with_all_history_is_unsupported() {
    assert_unsupported_combination(&["--at-head", "--all-history"]);
}

#[test]
fn at_head_with_at_is_unsupported() {
    assert_unsupported_combination(&["--at-head", "--at", "bbbb2222"]);
}

#[test]
fn at_head_with_as_of_is_unsupported() {
    assert_unsupported_combination(&["--at-head", "--as-of", "2026-03-01T00:00:00Z"]);
}

#[test]
fn all_history_with_at_is_unsupported() {
    assert_unsupported_combination(&["--all-history", "--at", "bbbb2222"]);
}

#[test]
fn all_history_with_as_of_is_unsupported() {
    assert_unsupported_combination(&["--all-history", "--as-of", "2026-03-01T00:00:00Z"]);
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_and_graph_agree_on_head_anchored_default() {
    let (_t, path, anchor_id, dep_a_id, dep_b_id) = seed_history_snapshot();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let ids_of = |rows: &[serde_json::Value]| -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .filter(|r| r["category"].as_str() == Some("dependency"))
            .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
            .collect();
        v.sort_unstable();
        v
    };

    let (graph_header, graph_rows) = run_query(&[
        "query",
        "deps",
        &anchor_id,
        "--graph",
        path.to_str().unwrap(),
    ]);
    let store_stdout = egregore()
        .args(["query", "deps", &anchor_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (store_header, store_rows) = parse_ndjson(&store_stdout);

    assert_eq!(graph_header["corpus_mode"], "head_anchored");
    assert_eq!(store_header["corpus_mode"], "head_anchored");
    let graph_ids = ids_of(&graph_rows);
    let store_ids = ids_of(&store_rows);
    assert_eq!(
        graph_ids, store_ids,
        "graph and store must agree on HEAD state"
    );
    assert!(!graph_ids.contains(&dep_a_id), "dep_a excluded on both");
    assert!(graph_ids.contains(&dep_b_id), "dep_b included on both");
}

/// Multi-repository history fixture: two repositories sharing one store.
/// Repo A ("repo-ha") has a single commit `aaaa1111` @ T1 where `anchor_ra`
/// calls `dep_ra`. Repo B ("repo-hb") has `bbbb2222` @ T2 (newer than
/// everything in repo A) and `aaaa2222` @ T1 (sharing repo A's `aaaa` prefix).
fn seed_multi_repo_history() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("multi-repo-history.jsonl");
    let mut graph = Graph::new();

    let repo_a_id = stable_id(&["node", "Repository", "repo-ha"]);
    let repo_b_id = stable_id(&["node", "Repository", "repo-hb"]);
    for (id, tag) in [(&repo_a_id, "repo-ha"), (&repo_b_id, "repo-hb")] {
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
    commit_in(&repo_a_id, "repo-ha", "aaaa1111", T1);
    commit_in(&repo_b_id, "repo-hb", "bbbb2222", T2);
    commit_in(&repo_b_id, "repo-hb", "aaaa2222", T1);

    // Repo A's tree at aaaa1111: anchor_ra calls dep_ra. The file/Defines
    // topology attributes both symbols to repo A for `--repo` scoping.
    file(&mut graph, &repo_a_id, "src/ra.rs");
    let hist_symbol = |graph: &mut Graph, path: &str, name: &str, sha: &str, vt: &str| -> String {
        let id = sym_id(path, name);
        graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Symbol,
                path.to_owned(),
                span(1, 10),
                name.to_owned(),
                "rust",
                format!("fn {name}"),
            )
            .with_temporal(temporal(sha, &[], vt)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id(path),
            id.clone(),
            None,
            format!("{path} defines {name}"),
        ));
        id
    };
    let anchor_a_id = hist_symbol(&mut graph, "src/ra.rs", "anchor_ra", "aaaa1111", T1);
    let dep_a_id = hist_symbol(&mut graph, "src/ra.rs", "dep_ra", "aaaa1111", T1);
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Calls,
            anchor_a_id.clone(),
            dep_a_id.clone(),
            Some("1.0".to_owned()),
            "anchor_ra calls dep_ra".to_owned(),
        )
        .with_resolution(CallResolution::Resolved)
        .with_temporal(temporal("aaaa1111", &[], T1)),
    );

    // Repo B's tree at bbbb2222: an unrelated symbol so the foreign commit
    // carries real records.
    file(&mut graph, &repo_b_id, "src/rb.rs");
    hist_symbol(&mut graph, "src/rb.rs", "anchor_rb", "bbbb2222", T2);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, anchor_a_id, dep_a_id)
}

#[test]
fn repo_scoped_as_of_resolves_within_selected_repository() {
    let (_t, path, anchor_a_id, dep_a_id) = seed_multi_repo_history();
    // At an instant after both repos' history, the globally newest commit is
    // repo B's bbbb2222. Scoped to repo A, the temporal view must resolve to
    // repo A's newest commit (aaaa1111) — not select the foreign commit and
    // then filter repo A's records away.
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_a_id,
        "--graph",
        path.to_str().unwrap(),
        "--repo",
        "repo-ha",
        "--as-of",
        "2026-03-01T00:00:00Z",
    ]);
    assert_eq!(
        header["at_commit"], "aaaa1111",
        "--as-of must resolve within the selected repository"
    );
    assert!(
        rows.iter()
            .filter_map(|r| r["record_id"].as_str())
            .any(|id| id == dep_a_id),
        "dep_ra answered at c1"
    );
}

#[test]
fn repo_scoped_at_prefix_ignores_other_repositories_commits() {
    let (_t, path, anchor_a_id, dep_a_id) = seed_multi_repo_history();
    // "aaaa" matches repo A's aaaa1111 and repo B's aaaa2222. Scoped to repo
    // A the prefix is unique and must not be reported ambiguous because of
    // commits outside the selected repository.
    let (header, rows) = run_query(&[
        "query",
        "deps",
        &anchor_a_id,
        "--graph",
        path.to_str().unwrap(),
        "--repo",
        "repo-ha",
        "--at",
        "aaaa",
    ]);
    assert_eq!(
        header["at_commit"], "aaaa1111",
        "--at prefix must resolve within the selected repository"
    );
    assert!(
        rows.iter()
            .filter_map(|r| r["record_id"].as_str())
            .any(|id| id == dep_a_id),
        "dep_ra answered at c1"
    );
}

#[test]
fn at_missing_commit_exit2() {
    let (_t, path, anchor_id, _a, _b) = seed_history();
    let stdout = egregore()
        .args([
            "query",
            "deps",
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
            "deps",
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
            "deps",
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
fn data_dir_store_returns_same_dependency_set() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    // The embedded sink rejects the fixture's intentionally dangling edge
    // (missing target node), so ingest a copy without it; the dependency set
    // differs only by that unresolved row.
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

    let stdout_graph = egregore()
        .args([
            "query",
            "deps",
            &f.anchor_id,
            "--graph",
            cleaned_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (_, graph_rows) = parse_ndjson(&stdout_graph);
    let stdout_store = egregore()
        .args(["query", "deps", &f.anchor_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (_, store_rows) = parse_ndjson(&stdout_store);

    let ids = |rows: &[serde_json::Value]| -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        ids(&graph_rows),
        ids(&store_rows),
        "graph and store views must agree"
    );
}

// ---------------------------------------------------------------------------
// Issue #424: the embedded `--data-dir` path must be strictly read-only.
// Opening the live engine re-persists index files, so the query must operate on
// a throwaway copy and leave the store byte-for-byte untouched — for both the
// current-state and the `--at`/`--as-of` history reads (mirrors `query path`).
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
fn snapshot_tree(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).expect("read file")));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_query_is_read_only() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    // Ingest a copy without the intentionally dangling edge (the embedded sink
    // rejects it); the read-only property under test is unaffected.
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

    let before = snapshot_tree(&data_dir);
    // Current-state read.
    egregore()
        .args(["query", "deps", &f.anchor_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    // History read (`--at` exercises the history-inclusive read-only loader). A
    // missing commit exits 2; irrelevant here — the point is no mutation.
    let _ = egregore()
        .args([
            "query",
            "deps",
            &f.anchor_id,
            "--at",
            "deadbeefdeadbeef",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert();
    let after = snapshot_tree(&data_dir);
    assert_eq!(
        before, after,
        "querying the embedded store must not create, modify, or delete any store file"
    );
}
