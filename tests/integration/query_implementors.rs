//! End-to-end tests for `eg query implementors <TRAIT>` (issue #133).
#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
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

/// Pushes a `Symbol` node with a `symbol_kind` plus the DEFINES edge from its file.
fn push_symbol(
    graph: &mut Graph,
    path: &str,
    name: &str,
    symbol_kind: &str,
    row_span: SourceSpan,
) -> String {
    let id = sym_id(path, name);
    graph.push(GraphRecord::syntax_symbol(
        id.clone(),
        symbol_kind,
        path.to_owned(),
        row_span,
        name.to_owned(),
        "rust",
        0,
        format!("Rust {symbol_kind} {name}"),
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

fn push_file(graph: &mut Graph, repo_id: &str, path: &str) -> String {
    let fid = file_id(path);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 200),
        path.rsplit('/').next().unwrap_or(path).to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        fid.clone(),
        None,
        format!("repo contains {path}"),
    ));
    fid
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Record IDs returned by `seed()` for assertions.
struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    trait_id: String,
    zero_trait_id: String,
    conflicted_a_id: String,
    conflicted_b_id: String,
    circle_id: String,
    square_id: String,
    widget_id: String,
    impl_circle_id: String,
    impl_square_id: String,
    impl_widget_id: String,
    other_impl_id: String,
    conflicted_a_impl_id: String,
    conflicted_b_impl_id: String,
    tombstoned_trait_id: String,
}

/// Builds a seeded JSONL fixture with:
/// - trait `Renderable` (src/render.rs) with exactly 3 recorded implementors
///   across 2 files: `Circle`, `Square` (src/shapes.rs) and `Widget`
///   (src/render.rs);
/// - an unrelated local trait `Other` with its own implementor (must never
///   bleed into `Renderable` answers);
/// - an `impl Display for Circle` block with NO `IMPLEMENTS` edge (external
///   trait — extraction records no edge);
/// - trait `Unimplemented` with zero `IMPLEMENTS` edges;
/// - trait `Conflicted` defined in two files (ambiguous name), one
///   implementor each;
/// - a tombstoned trait `Gone` (stale-handle case).
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("implementors.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-impl"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-impl".to_owned()),
        "Repository repo-impl".to_owned(),
    ));

    let render_path = "src/render.rs";
    let shapes_path = "src/shapes.rs";
    let a_mod_path = "src/a_mod.rs";
    let b_mod_path = "src/b_mod.rs";
    push_file(&mut graph, &repo_id, render_path);
    push_file(&mut graph, &repo_id, shapes_path);
    push_file(&mut graph, &repo_id, a_mod_path);
    push_file(&mut graph, &repo_id, b_mod_path);

    // ── Trait under test and its three implementors ─────────────────────────
    let trait_id = push_symbol(&mut graph, render_path, "Renderable", "trait", span(5, 12));
    let widget_id = push_symbol(&mut graph, render_path, "Widget", "struct", span(14, 16));
    let impl_widget_id = push_symbol(
        &mut graph,
        render_path,
        "impl Renderable for Widget",
        "impl",
        span(18, 26),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_widget_id.clone(),
        trait_id.clone(),
        None,
        "impl Renderable for Widget implementation relationship".to_owned(),
    ));

    let circle_id = push_symbol(&mut graph, shapes_path, "Circle", "struct", span(3, 5));
    let impl_circle_id = push_symbol(
        &mut graph,
        shapes_path,
        "impl Renderable for Circle",
        "impl",
        span(7, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_circle_id.clone(),
        trait_id.clone(),
        None,
        "impl Renderable for Circle implementation relationship".to_owned(),
    ));

    let square_id = push_symbol(&mut graph, shapes_path, "Square", "struct", span(17, 19));
    let impl_square_id = push_symbol(
        &mut graph,
        shapes_path,
        "impl Renderable for Square",
        "impl",
        span(21, 29),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_square_id.clone(),
        trait_id.clone(),
        None,
        "impl Renderable for Square implementation relationship".to_owned(),
    ));

    // ── Unrelated local trait with its own implementor ──────────────────────
    let other_trait_id = push_symbol(&mut graph, shapes_path, "Other", "trait", span(31, 34));
    let other_impl_id = push_symbol(
        &mut graph,
        shapes_path,
        "impl Other for Circle",
        "impl",
        span(36, 40),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        other_impl_id.clone(),
        other_trait_id,
        None,
        "impl Other for Circle implementation relationship".to_owned(),
    ));

    // ── External-trait impl: symbol exists, NO IMPLEMENTS edge ──────────────
    push_symbol(
        &mut graph,
        shapes_path,
        "impl Display for Circle",
        "impl",
        span(42, 48),
    );

    // ── Local trait with zero implementors ──────────────────────────────────
    let zero_trait_id = push_symbol(
        &mut graph,
        render_path,
        "Unimplemented",
        "trait",
        span(28, 31),
    );

    // ── Ambiguous trait name across two files ───────────────────────────────
    let conflicted_a_id = push_symbol(&mut graph, a_mod_path, "Conflicted", "trait", span(2, 6));
    let conflicted_b_id = push_symbol(&mut graph, b_mod_path, "Conflicted", "trait", span(2, 6));
    let a_type_id = push_symbol(&mut graph, a_mod_path, "Alpha", "struct", span(8, 9));
    let b_type_id = push_symbol(&mut graph, b_mod_path, "Beta", "struct", span(8, 9));
    let _ = (a_type_id, b_type_id);
    let conflicted_a_impl_id = push_symbol(
        &mut graph,
        a_mod_path,
        "impl Conflicted for Alpha",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        conflicted_a_impl_id.clone(),
        conflicted_a_id.clone(),
        None,
        "impl Conflicted for Alpha implementation relationship".to_owned(),
    ));
    let conflicted_b_impl_id = push_symbol(
        &mut graph,
        b_mod_path,
        "impl Conflicted for Beta",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        conflicted_b_impl_id.clone(),
        conflicted_b_id.clone(),
        None,
        "impl Conflicted for Beta implementation relationship".to_owned(),
    ));

    // ── Tombstoned trait ─────────────────────────────────────────────────────
    let tombstoned_trait_id = push_symbol(&mut graph, render_path, "Gone", "trait", span(40, 44));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &tombstoned_trait_id]),
        schema_version: aletheia_egregore::ir::SCHEMA_VERSION,
        deleted_id: tombstoned_trait_id.clone(),
        summary: "trait Gone was deleted".to_owned(),
        producer: None,
    });

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    Fixture {
        _temp: temp,
        graph: path,
        trait_id,
        zero_trait_id,
        conflicted_a_id,
        conflicted_b_id,
        circle_id,
        square_id,
        widget_id,
        impl_circle_id,
        impl_square_id,
        impl_widget_id,
        other_impl_id,
        conflicted_a_impl_id,
        conflicted_b_impl_id,
        tombstoned_trait_id,
    }
}

fn run_rows(fixture_graph: &PathBuf, name: &str) -> Vec<serde_json::Value> {
    let stdout = egregore()
        .args(["query", "implementors", name, "--graph"])
        .arg(fixture_graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    out.lines()
        .map(|line| serde_json::from_str(line).expect("each stdout line must be valid JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// AC2 — exactly the recorded implementors, none of the unrelated impls
// ---------------------------------------------------------------------------

#[test]
fn returns_exactly_recorded_implementors() {
    let f = seed();
    let rows = run_rows(&f.graph, "Renderable");
    assert_eq!(rows.len(), 3, "exactly 3 recorded implementors: {rows:?}");

    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(&f.impl_circle_id.as_str()));
    assert!(ids.contains(&f.impl_square_id.as_str()));
    assert!(ids.contains(&f.impl_widget_id.as_str()));
    assert!(
        !ids.contains(&f.other_impl_id.as_str()),
        "unrelated impl must not appear"
    );
}

// ---------------------------------------------------------------------------
// AC3 — every row carries a citable handle and the resolved implementing type
// ---------------------------------------------------------------------------

#[test]
fn rows_carry_citable_handles_and_resolved_types() {
    let f = seed();
    let rows = run_rows(&f.graph, "Renderable");

    for row in &rows {
        assert!(row["record_id"].as_str().is_some(), "impl record id: {row}");
        assert!(row["schema_version"].is_number(), "{row}");
        assert!(
            row["repo_relative_path"].as_str().is_some(),
            "file handle: {row}"
        );
        assert!(row["span"]["start_line"].is_number(), "span handle: {row}");
        assert_eq!(row["trait_record_id"], f.trait_id.as_str(), "{row}");
        assert_eq!(row["trait_name"], "Renderable", "{row}");
        assert!(row["edge_record_id"].as_str().is_some(), "{row}");
        assert_eq!(
            row["implementing_type_resolution"], "resolved",
            "type must be resolved from the graph, not just parsed: {row}"
        );
        assert_eq!(row["completeness"], "local_traits_only", "{row}");
    }

    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert!(types.contains(&"Circle"), "got: {types:?}");
    assert!(types.contains(&"Square"), "got: {types:?}");
    assert!(types.contains(&"Widget"), "got: {types:?}");

    let type_ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type_record_id"].as_str())
        .collect();
    assert!(type_ids.contains(&f.circle_id.as_str()));
    assert!(type_ids.contains(&f.square_id.as_str()));
    assert!(type_ids.contains(&f.widget_id.as_str()));
}

// ---------------------------------------------------------------------------
// Trait canonical record ID is accepted as the handle
// ---------------------------------------------------------------------------

#[test]
fn trait_record_id_handle_accepted() {
    let f = seed();
    let by_name = run_rows(&f.graph, "Renderable");
    let by_id = run_rows(&f.graph, &f.trait_id);
    assert_eq!(by_name, by_id, "record-id handle must match name handle");
}

// ---------------------------------------------------------------------------
// AC4 — zero implementors recorded is a signal, never a bare empty answer
// ---------------------------------------------------------------------------

#[test]
fn zero_implementors_signal_exit0() {
    let f = seed();
    let stdout = egregore()
        .args(["query", "implementors", "Unimplemented", "--graph"])
        .arg(&f.graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "one zero-signal line: {out}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["code"], "zero_implementors_recorded");
    assert_eq!(v["trait_record_id"], f.zero_trait_id.as_str());
    assert_eq!(v["trait_name"], "Unimplemented");
    assert_eq!(v["implementors_recorded"], 0);
    assert_eq!(v["completeness"], "local_traits_only");
    assert!(
        v["note"]
            .as_str()
            .is_some_and(|n| n.contains("external/std")),
        "zero answer must carry the local-traits-only incompleteness note: {v}"
    );
}

// ---------------------------------------------------------------------------
// AC4 — unresolved trait (e.g. external/std) is no_match, exit 2
// ---------------------------------------------------------------------------

#[test]
fn unresolved_trait_no_match_exit2() {
    let f = seed();
    let stdout = egregore()
        .args(["query", "implementors", "Display", "--graph"])
        .arg(&f.graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match", "got: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("locally")),
        "no-match must explain the locally-defined-traits-only bound: {v}"
    );
}

#[test]
fn tombstoned_trait_stale_handle_exit2() {
    let f = seed();
    let stdout = egregore()
        .args(["query", "implementors", "Gone", "--graph"])
        .arg(&f.graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "stale_handle", "got: {v}");
    let _ = f.tombstoned_trait_id;
}

// ---------------------------------------------------------------------------
// AC6 — ambiguous trait name returns ALL candidates labeled by trait handle
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_name_returns_all_candidates_labeled() {
    let f = seed();
    let rows = run_rows(&f.graph, "Conflicted");
    assert_eq!(rows.len(), 2, "one row per candidate trait: {rows:?}");

    let pairs: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| {
            (
                r["trait_record_id"].as_str().expect("trait_record_id"),
                r["record_id"].as_str().expect("record_id"),
            )
        })
        .collect();
    assert!(pairs.contains(&(f.conflicted_a_id.as_str(), f.conflicted_a_impl_id.as_str())));
    assert!(pairs.contains(&(f.conflicted_b_id.as_str(), f.conflicted_b_impl_id.as_str())));
}

// ---------------------------------------------------------------------------
// AC8 — deterministic byte-identical output across 5 runs
// ---------------------------------------------------------------------------

#[test]
fn deterministic_output_x5() {
    let f = seed();
    let run = || -> Vec<u8> {
        egregore()
            .args(["query", "implementors", "Renderable", "--graph"])
            .arg(&f.graph)
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
// AC8 — strictly read-only: no file is created, modified, or deleted
// ---------------------------------------------------------------------------

#[test]
fn read_only_no_writes() {
    let f = seed();
    let dir = f.graph.parent().expect("fixture dir").to_path_buf();
    let snapshot = |()| -> Vec<(String, Vec<u8>)> {
        let mut entries: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| {
                let e = e.expect("entry");
                let name = e.file_name().to_string_lossy().into_owned();
                let bytes = fs::read(e.path()).expect("read file");
                (name, bytes)
            })
            .collect();
        entries.sort();
        entries
    };
    let before = snapshot(());
    egregore()
        .args(["query", "implementors", "Renderable", "--graph"])
        .arg(&f.graph)
        .current_dir(&dir)
        .assert()
        .success();
    egregore()
        .args(["query", "implementors", "Unimplemented", "--graph"])
        .arg(&f.graph)
        .current_dir(&dir)
        .assert()
        .success();
    let after = snapshot(());
    assert_eq!(
        before, after,
        "query must not create/modify/delete any file"
    );
}

// ---------------------------------------------------------------------------
// AC7 — --format text works for rows and for the zero signal
// ---------------------------------------------------------------------------

#[test]
fn format_text_rows() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "Renderable",
            "--format",
            "text",
            "--graph",
        ])
        .arg(&f.graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(out.contains("Circle"), "text output names the type: {out}");
    assert!(out.contains("Renderable"), "text names the trait: {out}");
    assert!(out.contains("src/shapes.rs"), "text cites the file: {out}");
}

#[test]
fn format_text_zero_signal() {
    let f = seed();
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "Unimplemented",
            "--format",
            "text",
            "--graph",
        ])
        .arg(&f.graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(
        out.contains("0 implementors recorded"),
        "zero signal in text mode: {out}"
    );
}

#[test]
fn eg_alias_works() {
    let f = seed();
    eg().args(["query", "implementors", "Renderable", "--graph"])
        .arg(&f.graph)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// AC5 — temporal pinning: --at <commit> and --as-of <instant>
// ---------------------------------------------------------------------------

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: None,
    }
}

/// History-style fixture: at commit c1 the trait has one implementor; at c2 a
/// second implementor is added.
fn seed_history() -> (tempfile::TempDir, PathBuf, String, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");
    let mut graph = Graph::new();
    let c1 = "aaaa111111111111111111111111111111111111";
    let c2 = "bbbb222222222222222222222222222222222222";
    let t1 = "2026-01-01T00:00:00Z";
    let t2 = "2026-01-02T00:00:00Z";
    let p = "src/lib.rs";

    let trait_id = sym_id(p, "Pinned");
    let impl1_id = sym_id(p, "impl Pinned for One");
    let impl2_id = sym_id(p, "impl Pinned for Two");

    for (commit, vt) in [(c1, t1), (c2, t2)] {
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "Pinned".to_owned(),
                "rust",
                0,
                "Rust trait Pinned".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::syntax_symbol(
                impl1_id.clone(),
                "impl",
                p.to_owned(),
                span(6, 9),
                "impl Pinned for One".to_owned(),
                "rust",
                0,
                "Rust impl impl Pinned for One".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl1_id.clone(),
                trait_id.clone(),
                None,
                "impl Pinned for One implementation relationship".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
    }

    // Second implementor exists only at c2.
    graph.push(
        GraphRecord::syntax_symbol(
            impl2_id.clone(),
            "impl",
            p.to_owned(),
            span(11, 14),
            "impl Pinned for Two".to_owned(),
            "rust",
            0,
            "Rust impl impl Pinned for Two".to_owned(),
        )
        .with_temporal(temporal(c2, t2)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            impl2_id.clone(),
            trait_id.clone(),
            None,
            "impl Pinned for Two implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c2, t2)),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, impl1_id, impl2_id, trait_id)
}

#[test]
fn at_commit_pins_implementor_set() {
    let (_temp, graph, impl1_id, impl2_id, _trait_id) = seed_history();

    let at = |commit: &str| -> Vec<serde_json::Value> {
        let stdout = egregore()
            .args(["query", "implementors", "Pinned", "--at", commit, "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    };

    let rows_c1 = at("aaaa1111");
    assert_eq!(rows_c1.len(), 1, "one implementor at c1: {rows_c1:?}");
    assert_eq!(rows_c1[0]["record_id"], impl1_id.as_str());
    assert!(
        rows_c1[0]["git_commit"]
            .as_str()
            .is_some_and(|c| c.starts_with("aaaa")),
        "pinned row must cite the commit: {rows_c1:?}"
    );

    let rows_c2 = at("bbbb2222");
    assert_eq!(rows_c2.len(), 2, "two implementors at c2: {rows_c2:?}");
    let ids: Vec<&str> = rows_c2
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(&impl1_id.as_str()));
    assert!(ids.contains(&impl2_id.as_str()));
}

#[test]
fn as_of_pins_implementor_set() {
    let (_temp, graph, impl1_id, impl2_id, _trait_id) = seed_history();

    let as_of = |instant: &str| -> Vec<serde_json::Value> {
        let stdout = egregore()
            .args([
                "query",
                "implementors",
                "Pinned",
                "--as-of",
                instant,
                "--graph",
            ])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    };

    let rows_t1 = as_of("2026-01-01T12:00:00Z");
    assert_eq!(rows_t1.len(), 1, "one implementor as of t1: {rows_t1:?}");
    assert_eq!(rows_t1[0]["record_id"], impl1_id.as_str());

    let rows_t2 = as_of("2026-01-02T12:00:00Z");
    assert_eq!(rows_t2.len(), 2, "two implementors as of t2: {rows_t2:?}");
    assert!(
        rows_t2.iter().any(|r| r["record_id"] == impl2_id.as_str()),
        "impl2 must appear as of t2: {rows_t2:?}"
    );
}

#[test]
fn as_of_invalid_timestamp_exit1() {
    let f = seed();
    egregore()
        .args([
            "query",
            "implementors",
            "Renderable",
            "--as-of",
            "not-a-timestamp",
            "--graph",
        ])
        .arg(&f.graph)
        .assert()
        .code(1);
}

// ---------------------------------------------------------------------------
// AC1 — the embedded store path answers identically to the JSONL path
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_roundtrip_matches_graph() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&f.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "Renderable", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        3,
        "same 3 implementors via --data-dir: {rows:?}"
    );
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(&f.impl_circle_id.as_str()));
    assert!(ids.contains(&f.impl_square_id.as_str()));
    assert!(ids.contains(&f.impl_widget_id.as_str()));
}

// ---------------------------------------------------------------------------
// Real extraction end-to-end: scan a Rust source tree, then query.
// The extractor records IMPLEMENTS edges only when the trait definition is
// resolvable in the extraction scope; the external `Display` impl produces no
// edge and must not appear.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_roundtrip() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait Drawable {\n",
            "    fn draw(&self) -> String;\n",
            "}\n\n",
            "pub struct Point;\n",
            "pub struct Line;\n",
            "pub struct Polygon;\n\n",
            "impl Drawable for Point {\n",
            "    fn draw(&self) -> String {\n",
            "        String::new()\n",
            "    }\n",
            "}\n\n",
            "impl Drawable for Line {\n",
            "    fn draw(&self) -> String {\n",
            "        String::new()\n",
            "    }\n",
            "}\n\n",
            "impl Drawable for Polygon {\n",
            "    fn draw(&self) -> String {\n",
            "        String::new()\n",
            "    }\n",
            "}\n\n",
            "impl std::fmt::Display for Point {\n",
            "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n",
            "        write!(f, \"point\")\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "Drawable", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        3,
        "3 edge-backed implementors from a real scan: {rows:?}"
    );
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert!(types.contains(&"Point"), "got: {types:?}");
    assert!(types.contains(&"Line"), "got: {types:?}");
    assert!(types.contains(&"Polygon"), "got: {types:?}");
    for row in &rows {
        assert_eq!(row["repo_relative_path"], "src/lib.rs");
        assert!(row["span"]["start_line"].is_number());
    }
}

// ---------------------------------------------------------------------------
// Generic impl headers through the real scanner (issue #343): the extractor
// now trait-edge-backs same-file generic trait impl headers. `impl<T> Trait
// for Type<T>` (generic binder) and `impl Trait<Args> for Type` (a generic
// trait instantiation whose trait segment carries generic args) both resolve
// to the local trait and surface as edge-backed implementor rows. Two forms
// stay deliberately bounded out: an inherent generic impl (`impl<T> Type<T>`,
// no `for` clause) keeps its self-referential record edge and never surfaces
// as a trait implementor, and a blanket impl (`impl<T> Trait for T`, whose
// `for` target is a bare binder type parameter) mints no edge at all. The
// query still reports the `local_traits_only` completeness bound because
// cross-file trait resolution (issue #344) remains out of this slice.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn real_scan_generic_trait_impls_are_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait GenT {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub trait GenP<T> {\n",
            "    fn put(&self, value: T);\n",
            "}\n\n",
            "pub trait Blanket {\n",
            "    fn any(&self);\n",
            "}\n\n",
            "pub struct Wrapper<T> {\n",
            "    inner: T,\n",
            "}\n\n",
            "pub struct Plain;\n\n",
            // Generic binder, trait segment `GenT`, non-parameter RHS:
            // newly edge-backed to `GenT` (implementing type `Wrapper`).
            "impl<T> GenT for Wrapper<T> {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            // Non-generic trait impl (already edge-backed).
            "impl GenT for Plain {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            // Generic-trait instantiation: trait segment `GenP<u32>` strips to
            // `GenP` and newly edge-backs (implementing type `Plain`).
            "impl GenP<u32> for Plain {\n",
            "    fn put(&self, _value: u32) {}\n",
            "}\n\n",
            // Blanket impl: RHS `for T` is a bare binder parameter -> no edge.
            "impl<T> Blanket for T {\n",
            "    fn any(&self) {}\n",
            "}\n\n",
            // Inherent generic impl: no `for` clause -> keeps its self edge,
            // never a trait implementor.
            "impl<T> Wrapper<T> {\n",
            "    fn inner_ref(&self) -> &T {\n",
            "        &self.inner\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // GenT: both the non-generic impl (`Plain`) and the generic binder impl
    // (`Wrapper`) are now edge-backed.
    let stdout = egregore()
        .args(["query", "implementors", "GenT", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "both the non-generic and generic trait impls are edge-backed: {rows:?}"
    );
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert!(types.contains(&"Plain"), "got: {types:?}");
    assert!(types.contains(&"Wrapper"), "got: {types:?}");
    for row in &rows {
        assert_eq!(row["completeness"], "local_traits_only");
    }

    // GenP (generic trait): its instantiation `impl GenP<u32> for Plain` now
    // strips the trait-segment generics and edge-backs to `GenP`.
    let stdout = egregore()
        .args(["query", "implementors", "GenP", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the generic-trait instantiation is edge-backed: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "Plain");
    assert_eq!(rows[0]["completeness"], "local_traits_only");

    // Blanket (`impl<T> Blanket for T`): the RHS is a bare binder parameter,
    // so no IMPLEMENTS edge is minted and the trait answers the explicit zero
    // signal — never a fabricated blanket implementor row.
    let stdout = egregore()
        .args(["query", "implementors", "Blanket", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "one zero-signal envelope: {rows:?}");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[0]["code"], "zero_implementors_recorded");
    assert_eq!(rows[0]["trait_name"], "Blanket");
    assert_eq!(rows[0]["implementors_recorded"], 0);
    assert_eq!(rows[0]["completeness"], "local_traits_only");
    assert!(
        rows[0]["note"].as_str().is_some_and(|n| !n.is_empty()),
        "zero signal must carry the incompleteness note: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// A generic trait impl whose binder carries a function-trait bound with a
// return arrow -- `impl<T: Fn() -> u32> Target for Wrapper<T>` -- must still
// edge-back to `Target`. The `->` in the bound is the regression trigger for
// the pre-AST char-scan binder split: `split_generic_binder` closed depth on
// the `>` of `->`, so the header remainder became `u32> Target for Wrapper<T>`
// and the trait segment resolved to garbage, minting no IMPLEMENTS edge. The
// Tree-sitter `type_parameters` field bounds the binder structurally, so the
// return arrow can never leak into the trait segment.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_generic_impl_with_arrow_bound_is_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait Target {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub struct Wrapper<T>(T);\n\n",
            // Generic binder with a function-trait bound whose signature has a
            // return arrow. The `->` must not break the binder/trait split;
            // the trait segment is `Target` and the impl edge-backs.
            "impl<T: Fn() -> u32> Target for Wrapper<T> {\n",
            "    fn go(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "Target", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the arrow-bound generic trait impl is edge-backed: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "Wrapper");
    assert_eq!(rows[0]["trait_name"], "Target");
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// A blanket impl whose `for` target is a REFERENCE (or pointer) to a bare
// binder type parameter -- `impl<T> RefTrait for &T` (also `&mut T`, `&'a T`)
// -- is still a blanket impl: it covers every type, has no single concrete
// implementing-type record, and must stay bounded out (issue #343's "blanket
// impls bounded out" decision). Before the fix the sigil (`&`) broke the bare
// `T` filter, so `&T` fell through to `Resolve` and fabricated an IMPLEMENTS
// edge with no concrete type record. Only the concrete `impl RefTrait for
// Owned` edge-backs; the reference/pointer blanket impls mint no edge.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn real_scan_reference_blanket_trait_impls_are_bounded_out() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait RefTrait {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub struct Owned;\n\n",
            // Concrete nominal impl: the only edge-backed implementor.
            "impl RefTrait for Owned {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            // Blanket over a shared reference to a binder param -> no edge.
            "impl<T> RefTrait for &T {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            // Blanket over a mutable reference to a binder param -> no edge.
            "impl<T> RefTrait for &mut T {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            // Blanket over a lifetime-annotated reference -> no edge.
            "impl<'a, T> RefTrait for &'a T {\n",
            "    fn go(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "RefTrait", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "only the concrete `impl RefTrait for Owned` is edge-backed; the \
         reference/pointer blanket impls mint no fabricated implementor: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "Owned");
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// A negative impl (`impl !LocalAuto for Foo`) asserts that the type explicitly
// does NOT implement the trait. Tree-sitter keeps the `!` token OUTSIDE the
// `trait` field, so the AST path (issue #343/#344 refactor) read the trait as
// `LocalAuto` and resolved it like a positive impl -- minting a WRONG IMPLEMENTS
// edge so `query implementors LocalAuto` reported a type that does not
// implement it. A negative impl must mint no edge; only the concrete positive
// `impl LocalAuto for Bar` edge-backs.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_negative_impl_emits_no_implements_edge() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    // The extractor only PARSES this source (Tree-sitter), never compiles it, so
    // the `negative_impls` feature gate is irrelevant to extraction.
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait LocalAuto {}\n\n",
            "pub struct Foo;\n",
            "pub struct Bar;\n\n",
            // Negative impl: Foo explicitly does NOT implement LocalAuto -> no
            // edge, so Foo must never surface as an implementor.
            "impl !LocalAuto for Foo {}\n\n",
            // Positive impl: the only edge-backed implementor.
            "impl LocalAuto for Bar {}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "LocalAuto", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "only the positive `impl LocalAuto for Bar` edge-backs; the negative \
         `impl !LocalAuto for Foo` mints no implementor: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "Bar");
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert!(
        !types.contains(&"Foo"),
        "a negative impl must never report the type as an implementor: {types:?}"
    );
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// Turbofish trait syntax (`impl GenP::<u32> for Plain`, valid Rust in type
// position) leaves a trailing `::` separator once the trait-segment generic
// args are stripped. Before the fix the trait normalized to `GenP::`, which
// the local (#343) resolver treated as a qualified path that could not match
// the bare `GenP` trait, so no IMPLEMENTS edge was minted. The trailing
// turbofish `::` is now dropped, so the same-file `impl GenP::<u32> for Plain`
// edge-backs to `GenP`, exactly like the non-turbofish `impl GenP<u32>` form.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_turbofish_trait_impl_is_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait GenP<T> {\n",
            "    fn put(&self, value: T);\n",
            "}\n\n",
            "pub struct Plain;\n\n",
            // Turbofish trait instantiation: trait segment `GenP::<u32>`
            // normalizes to `GenP` and edge-backs (implementing type `Plain`).
            "impl GenP::<u32> for Plain {\n",
            "    fn put(&self, _value: u32) {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "GenP", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the turbofish trait instantiation is edge-backed: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "Plain");
    assert_eq!(rows[0]["trait_name"], "GenP");
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// Cross-file turbofish (#344): the trait is defined in the crate root and the
// impl in a separate `mod m;` file names it through a qualified turbofish path
// `crate::GenP::<u32>`. The trailing turbofish `::` is dropped while the
// internal `crate::` path separators are preserved (`crate::GenP`), so the
// repo-wide `cross_file_implements_records` pass resolves the root trait and
// edge-backs the implementor.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_turbofish_trait_is_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait GenP<T> {\n",
            "    fn put(&self, value: T);\n",
            "}\n\n",
            "pub mod m;\n",
        ),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("m.rs"),
        concat!(
            "pub struct Foo;\n\n",
            "impl crate::GenP::<u32> for Foo {\n",
            "    fn put(&self, _value: u32) {}\n",
            "}\n",
        ),
    )
    .expect("write m.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "GenP", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one cross-file turbofish implementor row (Foo -> GenP): {rows:?}"
    );
    assert_eq!(
        rows[0]["implementing_type"], "m::Foo",
        "the out-of-line `impl crate::GenP::<u32> for Foo` edge-backs (trailing \
         turbofish `::` dropped, internal path preserved): {rows:?}"
    );
    assert_eq!(rows[0]["trait_name"], "GenP");
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// Multi-crate-root conservative bound (issue #344 review): when a package has
// two crate roots (`src/lib.rs` + `src/bin/tool.rs`), a root `trait T` in each
// gets the SAME crate-root-relative qualified name `T`. The repo-wide impl
// index keys only on that qualified name, so `crate::T` matches BOTH root
// definitions. The resolver must leave that multi-candidate match UNRESOLVED
// (mint no edge) rather than silently pick one and mis-target a cross-root
// edge — a lib `impl crate::T for Foo` must never resolve to the binary's `T`.
// This is a GREEN regression guard: the resolver already resolves only unique
// matches (`[only]`), so no behavior change was needed. Full crate-root
// partitioning of the index is a deferred follow-up, not this slice.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_multi_crate_root_same_name_trait_is_unresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    let bin = src.join("bin");
    fs::create_dir_all(&bin).expect("mkdir src/bin");
    // Library crate root defines `trait T` and an out-of-line `impl crate::T
    // for Foo` in a submodule (forcing the repo-wide cross-file resolver).
    fs::write(
        src.join("lib.rs"),
        concat!("pub trait T {}\n", "pub mod m;\n"),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("m.rs"),
        concat!("pub struct Foo;\n", "impl crate::T for Foo {}\n"),
    )
    .expect("write m.rs");
    // Binary crate root ALSO defines a root `trait T` -> same qualified name.
    fs::write(
        bin.join("tool.rs"),
        concat!("pub trait T {}\n", "fn main() {}\n"),
    )
    .expect("write bin/tool.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // No IMPLEMENTS edge whatsoever names `Foo`: the graph must carry zero
    // cross-root edges. (A raw text scan of the graph is the tightest guard.)
    let graph_text = fs::read_to_string(&graph_path).expect("read graph");
    for line in graph_text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert!(
            v["label"] != "IMPLEMENTS",
            "no IMPLEMENTS edge may be minted for the ambiguous cross-root `crate::T`: {line}"
        );
    }

    // The query answers the zero-implementors signal for the trait(s) named
    // `T`; `Foo` must never appear as an implementor of either crate root's `T`.
    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    assert!(
        !out.contains("Foo"),
        "a multi-crate-root same-name trait must not mis-target `Foo`: {out}"
    );
    for line in out.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(
            v["code"], "zero_implementors_recorded",
            "each crate root's `T` reports zero implementors, never a mis-targeted row: {line}"
        );
        assert_eq!(v["implementors_recorded"], 0, "{line}");
    }
}

// ---------------------------------------------------------------------------
// `unsafe impl` of a local trait is edge-backed: `unsafe ` is a transparent
// keyword prefix on the header, so both the non-generic
// `unsafe impl Zeroable for Foo` and — as of issue #343 — the same-file
// generic `unsafe impl<T> Zeroable for Bar<T>` record their IMPLEMENTS edges
// to the trait, and the query resolves each implementing type.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_unsafe_impl_is_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub unsafe trait Zeroable {\n",
            "    fn zeroed() -> Self;\n",
            "}\n\n",
            "pub struct Foo;\n\n",
            "pub struct Bar<T> {\n",
            "    inner: T,\n",
            "}\n\n",
            "unsafe impl Zeroable for Foo {\n",
            "    fn zeroed() -> Self {\n",
            "        Foo\n",
            "    }\n",
            "}\n\n",
            "unsafe impl<T> Zeroable for Bar<T> {\n",
            "    fn zeroed() -> Self {\n",
            "        unreachable!()\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "Zeroable", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    // Both the non-generic `unsafe impl Zeroable for Foo` and the generic
    // `unsafe impl<T> Zeroable for Bar<T>` are edge-backed: `unsafe ` is
    // transparent to header normalization, and same-file generic trait impls
    // now trait-edge-back (issue #343).
    assert_eq!(
        rows.len(),
        2,
        "both the non-generic and generic unsafe impls are edge-backed: {rows:?}"
    );
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert!(types.contains(&"Foo"), "got: {types:?}");
    assert!(types.contains(&"Bar"), "got: {types:?}");
    let foo_row = rows
        .iter()
        .find(|r| r["implementing_type"] == "Foo")
        .expect("Foo row");
    assert_eq!(foo_row["name"], "unsafe impl Zeroable for Foo");
    assert_eq!(foo_row["implementing_type_resolution"], "resolved");
    for row in &rows {
        assert_eq!(row["completeness"], "local_traits_only");
    }
}

// ---------------------------------------------------------------------------
// Qualified local trait paths are edge-backed (PR #296 review): `impl
// crate::T for X`, `impl self::T for X`, and `impl super::T for X` name a
// locally-defined trait through a path qualifier — ordinary Rust, not the
// generic/blanket extraction-deepening issue #133 scopes out. The extractor
// must normalize the qualifier against the impl's module scope before the
// trait lookup, so these impls edge-back exactly like their plain-path
// forms.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_qualified_local_trait_paths_are_edge_backed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait Drawable {\n",
            "    fn draw(&self);\n",
            "}\n\n",
            "pub struct Point;\n",
            "pub struct Dot;\n\n",
            "impl crate::Drawable for Point {\n",
            "    fn draw(&self) {}\n",
            "}\n\n",
            "impl self::Drawable for Dot {\n",
            "    fn draw(&self) {}\n",
            "}\n\n",
            "pub mod m {\n",
            "    pub trait Inner {\n",
            "        fn go(&self);\n",
            "    }\n\n",
            "    pub struct Foo;\n",
            "    pub struct Baz;\n",
            "    pub struct Qux;\n\n",
            "    impl self::Inner for Foo {\n",
            "        fn go(&self) {}\n",
            "    }\n\n",
            "    impl crate::m::Inner for Baz {\n",
            "        fn go(&self) {}\n",
            "    }\n\n",
            "    impl super::Drawable for Qux {\n",
            "        fn draw(&self) {}\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Drawable: crate:: at the root, self:: at the root, and super:: from
    // inside the module all resolve to the same local trait.
    let stdout = egregore()
        .args(["query", "implementors", "Drawable", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert_eq!(
        rows.len(),
        3,
        "crate::/self::/super:: paths all edge-back: {rows:?}"
    );
    assert!(types.contains(&"Point"), "crate:: at root: {types:?}");
    assert!(types.contains(&"Dot"), "self:: at root: {types:?}");
    assert!(types.contains(&"m::Qux"), "super:: from module: {types:?}");

    // m::Inner: self:: and crate::m:: inside the module both edge-back.
    let stdout = egregore()
        .args(["query", "implementors", "m::Inner", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "self:: and crate::m:: inside the module edge-back: {rows:?}"
    );
    assert!(types.contains(&"m::Foo"), "self:: in module: {types:?}");
    assert!(types.contains(&"m::Baz"), "crate::m:: in module: {types:?}");
}

// ---------------------------------------------------------------------------
// Crate-root trait paths resolve absolutely (PR #296 review): `definitions`
// aliases every symbol under its bare name, and a nested `m::T` inserted
// later overwrites the bare `T` alias of a root trait. A `crate::T` (or
// root-resolved `self::`/`super::`) lookup must consult only the
// fully-qualified key space, so the edge lands on the root trait — never
// the nested same-named one.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_crate_root_trait_path_never_hits_nested_alias() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod m {\n",
            "    pub trait T {\n",
            "        fn go(&self);\n",
            "    }\n",
            "}\n\n",
            "pub struct Foo;\n\n",
            "impl crate::T for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The root trait T owns the implementor.
    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "crate::T resolves the root trait: {rows:?}");
    assert_eq!(rows[0]["implementing_type"], "Foo");
    assert_eq!(rows[0]["trait_name"], "T");

    // The nested m::T gains no false implementor.
    let stdout = egregore()
        .args(["query", "implementors", "m::T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "one zero-signal envelope: {rows:?}");
    assert_eq!(
        rows[0]["code"], "zero_implementors_recorded",
        "the nested trait must not steal the root trait's implementor: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Unqualified impl traits resolve in the impl's module scope (PR #296
// review): the bare-alias `definitions` lookup let a later-inserted nested
// `m::T` shadow the root trait for a ROOT-LEVEL `impl T for Foo`. Unqualified
// trait names must resolve module-scope-first — the impl's own module, then
// outward to the crate root — through the qualified-only key space, with the
// bare alias only as a final fallback (use-imported cross-module names).
// ---------------------------------------------------------------------------

#[test]
fn real_scan_unqualified_trait_resolves_in_impl_module_scope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod m {\n",
            "    pub trait T {\n",
            "        fn go(&self);\n",
            "    }\n\n",
            "    pub struct MFoo;\n\n",
            "    impl T for MFoo {\n",
            "        fn go(&self) {}\n",
            "    }\n",
            "}\n\n",
            "pub struct Foo;\n\n",
            "impl T for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            "pub trait Out {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod k {\n",
            "    use super::Out;\n\n",
            "    pub struct Kid;\n\n",
            "    impl Out for Kid {\n",
            "        fn go(&self) {}\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let types_for = |trait_handle: &str| -> Vec<String> {
        let stdout = egregore()
            .args(["query", "implementors", trait_handle, "--graph"])
            .arg(&graph_path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON"))
            .filter_map(|r| r["implementing_type"].as_str().map(str::to_owned))
            .collect()
    };

    // Root impl resolves the root trait — never m::T's bare alias. (No `use` of
    // `T` is in scope, so the import-shadow veto does not fire here.)
    let root = types_for("T");
    assert_eq!(root, vec!["Foo".to_owned()], "root impl -> root trait");

    // Same-module impl resolves its own module's trait. (No `use` of `T` in
    // scope either, so the veto does not fire.)
    let nested = types_for("m::T");
    assert_eq!(
        nested,
        vec!["m::MFoo".to_owned()],
        "module impl -> module trait"
    );

    // Round-9 reconciliation: `mod k { use super::Out; impl Out for Kid }`
    // shadows the bare `Out` with a `use` in scope `k`, so the AST-derived
    // import-shadow veto (issues #343/#344) now leaves the impl UNRESOLVED
    // rather than walking outward to the root `Out`. This is the documented
    // recall trade-off — a `use` of the resolved name (even when the import
    // target IS the local trait) is left unresolved; correct import-aware
    // resolution is follow-up #393. The root `Out` therefore gains no
    // implementor.
    let outward = types_for("Out");
    assert!(
        outward.is_empty(),
        "import-shadowed bare `impl Out for Kid` is left unresolved (#393): {outward:?}"
    );
}

// ---------------------------------------------------------------------------
// Item order is insignificant in Rust (PR #296 review): a trait defined
// AFTER the impl that names it must still edge-back. The extractor resolves
// impl trait lookups after the whole file is indexed, so
// `struct Foo; impl LaterT for Foo {} trait LaterT {}` records the same
// IMPLEMENTS edge as the trait-first ordering — for plain, qualified, and
// module-nested forms alike.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_trait_defined_after_impl_still_edge_backs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub struct Foo;\n\n",
            "impl LaterT for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            "pub trait LaterT {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub struct Bar;\n\n",
            "impl crate::LaterQ for Bar {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            "pub trait LaterQ {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod m {\n",
            "    pub struct MKid;\n\n",
            "    impl LaterM for MKid {\n",
            "        fn go(&self) {}\n",
            "    }\n\n",
            "    pub trait LaterM {\n",
            "        fn go(&self);\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let types_for = |trait_handle: &str| -> Vec<String> {
        let stdout = egregore()
            .args(["query", "implementors", trait_handle, "--graph"])
            .arg(&graph_path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON"))
            .filter_map(|r| r["implementing_type"].as_str().map(str::to_owned))
            .collect()
    };

    assert_eq!(
        types_for("LaterT"),
        vec!["Foo".to_owned()],
        "plain trait-after-impl edge-backs"
    );
    assert_eq!(
        types_for("LaterQ"),
        vec!["Bar".to_owned()],
        "crate:: trait-after-impl edge-backs"
    );
    assert_eq!(
        types_for("m::LaterM"),
        vec!["m::MKid".to_owned()],
        "module-nested trait-after-impl edge-backs"
    );
}

// ---------------------------------------------------------------------------
// Relative qualified trait paths resolve in module scope (PR #296 review):
// `impl sibling::T for Foo` inside `mod m` names the local trait
// `m::sibling::T` — a relative path, not crate::/self::/super::. The
// extractor walks the impl's module scope outward (m::sibling::T, then
// sibling::T at the root), mirroring the unqualified scope walk; cross-crate
// paths (std::fmt::Display) still match nothing and emit no edge.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_relative_qualified_trait_path_resolves_in_module_scope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub mod m {\n",
            "    pub mod sibling {\n",
            "        pub trait T {\n",
            "            fn go(&self);\n",
            "        }\n",
            "    }\n\n",
            "    pub struct Foo;\n\n",
            "    impl sibling::T for Foo {\n",
            "        fn go(&self) {}\n",
            "    }\n",
            "}\n\n",
            "pub mod root_sib {\n",
            "    pub trait R {\n",
            "        fn go(&self);\n",
            "    }\n",
            "}\n\n",
            "pub struct Bar;\n\n",
            "impl root_sib::R for Bar {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            "pub struct Ext;\n\n",
            "impl std::fmt::Display for Ext {\n",
            "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n",
            "        write!(f, \"ext\")\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let types_for = |trait_handle: &str| -> Vec<String> {
        let stdout = egregore()
            .args(["query", "implementors", trait_handle, "--graph"])
            .arg(&graph_path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON"))
            .filter_map(|r| r["implementing_type"].as_str().map(str::to_owned))
            .collect()
    };

    // Sibling path inside a module resolves to the module-scoped trait.
    assert_eq!(
        types_for("m::sibling::T"),
        vec!["m::Foo".to_owned()],
        "module-scoped sibling path edge-backs"
    );

    // Root-level relative path keeps resolving as before.
    assert_eq!(
        types_for("root_sib::R"),
        vec!["Bar".to_owned()],
        "root-level relative path edge-backs"
    );

    // The external std:: impl never edge-backs: no local Display symbol
    // exists, so the handle is an honest no_match — never a fabricated row.
    let stdout = egregore()
        .args(["query", "implementors", "Display", "--graph"])
        .arg(&graph_path)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .next()
            .expect("one line"),
    )
    .expect("valid JSON");
    assert_eq!(v["error"]["code"], "no_match", "got: {v}");
}

// ---------------------------------------------------------------------------
// The impl trait resolver never crosses into the value namespace (PR #296
// review): with `mod m { trait T }`, `use m::T;`, `impl T for Foo`, and a
// later `fn T()`, the function overwrites the bare `T` alias in the general
// reference-definition map. The resolver must never bind an IMPLEMENTS edge to
// that value-namespace `fn T` — the standing invariant this test guards.
//
// Round-9 reconciliation: the bare `impl T for Foo` is shadowed by `use m::T;`
// in its module scope, so the AST-derived import-shadow veto (issues
// #343/#344) now leaves it UNRESOLVED rather than resolving it to `m::T` — the
// documented recall trade-off (a `use` of the resolved name, even when the
// import target IS the local trait, is left unresolved; correct import-aware
// resolution is follow-up #393). `m::T` therefore gains no implementor. The
// value-namespace guard below is unaffected and remains the load-bearing
// assertion.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_use_imported_trait_beats_value_namespace_shadow() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub mod m {\n",
            "    pub trait T {\n",
            "        fn go(&self);\n",
            "    }\n",
            "}\n\n",
            "use m::T;\n\n",
            "pub struct Foo;\n\n",
            "impl T for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n\n",
            "#[allow(non_snake_case)]\n",
            "pub fn T() {}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The bare `impl T for Foo` is shadowed by `use m::T;` in scope, so the
    // import-shadow veto leaves it unresolved (recall trade-off, #393): `m::T`
    // gains no implementor rather than mint an edge for a name that syntactically
    // refers to the import.
    let stdout = egregore()
        .args(["query", "implementors", "m::T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert!(
        rows.iter()
            .all(|r| r["implementing_type"] != "Foo" && r["implementing_type"] != "m::Foo"),
        "the import-shadowed bare impl is left unresolved (recall trade-off, #393): {rows:?}"
    );

    // The value-namespace fn named T never receives an IMPLEMENTS edge — the
    // load-bearing invariant, unaffected by the veto.
    let graph_text = fs::read_to_string(&graph_path).expect("read graph");
    let fn_id = graph_text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["symbol_kind"] == "function" && r["name"] == "T")
        .map(|r| r["id"].as_str().expect("id").to_owned())
        .expect("fn T record");
    let fn_is_implements_target = graph_text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|r| {
            r["record_type"] == "edge"
                && r["label"] == "IMPLEMENTS"
                && (r["target"] == fn_id.as_str() || r["target_id"] == fn_id.as_str())
        });
    assert!(
        !fn_is_implements_target,
        "the value-namespace fn must never be an IMPLEMENTS target"
    );
}

// ---------------------------------------------------------------------------
// Cross-file trait resolution is honestly bounded (PR #296 review + issue
// #133 scope): the extractor resolves impl trait lookups within one file's
// definitions; a trait living in another file (the out-of-line module
// layout: lib.rs defines the trait, m.rs holds `impl crate::T for Foo`) is
// not edge-backed in this slice. Cross-file trait resolution needs a
// repo-wide type-symbol index — extraction-deepening, a follow-up. The
// query must answer with the documented honest zero, never a fabricated row.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_trait_is_edge_backed() {
    // Out-of-line module layout (issue #344): the trait is defined in the
    // crate root and the impl lives in a separate `mod m;` file that names it
    // through `crate::T`. Per-file extraction resolves impls only against the
    // file it is walking, so before #344 this emitted ZERO edges; the repo-wide
    // `cross_file_implements_records` pass now edge-backs it.
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod m;\n",
        ),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("m.rs"),
        concat!(
            "pub struct Foo;\n\n",
            "impl crate::T for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write m.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one cross-file implementor row (Foo -> T): {rows:?}"
    );
    assert_eq!(
        rows[0]["implementing_type"], "m::Foo",
        "the out-of-line `impl crate::T for Foo` edge-backs to Foo (resolved \
         to its module-qualified name): {rows:?}"
    );
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// Cross-file out-of-line resolution also handles an UNQUALIFIED trait name
// (issue #344): an `impl Draw for Button` inside a separate `mod widgets;`
// file resolves outward through the module scope to the crate-root `Draw`
// trait, exactly like the same-file unqualified scope walk. Re-scanning after
// editing one side re-derives the edge (parity with the CALLS pass), because
// the pass recomputes from `FileFacts` every scan.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_unqualified_trait_resolves_outward() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait Draw {\n",
            "    fn draw(&self);\n",
            "}\n\n",
            "pub mod widgets;\n",
        ),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("widgets.rs"),
        concat!(
            "pub struct Button;\n\n",
            "impl Draw for Button {\n",
            "    fn draw(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write widgets.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "Draw", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one cross-file implementor row (Button -> Draw): {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "widgets::Button");
    assert_eq!(rows[0]["completeness"], "local_traits_only");
}

// ---------------------------------------------------------------------------
// Ambiguous bare (unqualified) trait names are left UNRESOLVED, never
// mis-bound to a root same-named trait (Codex review finding on #344). When an
// out-of-line module imports a NON-ROOT trait and implements it by bare name
// (`use crate::a::T; impl T for Foo`) while the crate root ALSO defines a
// same-named trait, the module-scope outward walk reaches the root `T` at
// depth 0 and would emit a WRONG IMPLEMENTS edge (root `T` gaining `Foo`, and
// `a::T` losing its implementor). The documented `local_traits_only` bound says
// use-alias / non-root bare trait paths stay unresolved, and a wrong-target
// edge is worse than a missing one, so this bare-name reference — ambiguous by
// simple name across the repo trait index — mints NO edge to the root `T`.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_bare_imported_trait_is_not_misresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {\n",
            "    fn go(&self);\n",
            "}\n\n",
            "pub mod a;\n",
            "pub mod m;\n",
        ),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("a.rs"),
        concat!("pub trait T {\n", "    fn go(&self);\n", "}\n"),
    )
    .expect("write a.rs");
    fs::write(
        src.join("m.rs"),
        concat!(
            "use crate::a::T;\n\n",
            "pub struct Foo;\n\n",
            "impl T for Foo {\n",
            "    fn go(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write m.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    // The bare `T` in m.rs is a `use crate::a::T` alias whose simple name is
    // ambiguous across the repo (root `T` and `a::T`), so it stays unresolved:
    // `Foo` must NOT appear as an implementor of any `T`.
    assert!(
        rows.iter()
            .all(|r| r["implementing_type"] != "m::Foo" && r["implementing_type"] != "Foo"),
        "bare imported trait must not mis-bind `Foo` to a same-named trait: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// A bare (unqualified) INHERENT impl whose type is defined in another file must
// not mis-resolve to an unrelated same-named type (Codex round-7 finding). A
// non-generic inherent impl (`impl Foo {}`) carries the TYPE name `Foo` as its
// pending trait path; when the module imports `use crate::a::Foo` and the crate
// root ALSO defines a same-named `struct Foo`, the module-scope outward walk
// reaches the root `Foo` at depth 0 and would emit a WRONG IMPLEMENTS edge
// (root `Foo` gaining `m::Foo` as an implementor). The ambiguity guard must
// count ALL impl-target kinds (traits AND type-defining targets), not only
// traits, so this bare type reference — ambiguous by simple name across the
// repo impl-target index (root `Foo` and `a::Foo`) — mints NO edge.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_bare_inherent_impl_is_not_misresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!("pub struct Foo;\n\n", "pub mod a;\n", "pub mod m;\n",),
    )
    .expect("write lib.rs");
    fs::write(src.join("a.rs"), "pub struct Foo;\n").expect("write a.rs");
    fs::write(
        src.join("m.rs"),
        concat!(
            "use crate::a::Foo;\n\n",
            "impl Foo {\n",
            "    pub fn x(&self) {}\n",
            "}\n",
        ),
    )
    .expect("write m.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let graph = fs::read_to_string(&graph_path).expect("read graph");
    let records: Vec<serde_json::Value> = graph
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();

    // The root `struct Foo` node — the bare inherent impl in m.rs must NOT bind
    // to it.
    let root_foo_id = records
        .iter()
        .find(|r| r["record_type"] == "node" && r["symbol_kind"] == "struct" && r["name"] == "Foo")
        .and_then(|r| r["id"].as_str().map(str::to_owned))
        .expect("root struct Foo node present");

    // The bare inherent impl `impl Foo {}` in m.rs is a `use crate::a::Foo`
    // alias whose simple name is ambiguous across the repo impl-target index
    // (root `Foo` and `a::Foo`), so it stays UNRESOLVED: no IMPLEMENTS edge may
    // target the root `Foo`.
    let implements_targets: Vec<&str> = records
        .iter()
        .filter(|r| r["record_type"] == "edge" && r["label"] == "IMPLEMENTS")
        .filter_map(|r| r["target"].as_str())
        .collect();
    assert!(
        !implements_targets.contains(&root_foo_id.as_str()),
        "bare inherent impl must not mis-bind to the root struct Foo: {implements_targets:?}"
    );
    // Stronger bound: the ambiguous bare inherent impl resolves to no
    // impl-target at all, so no cross-file IMPLEMENTS edge exists to either Foo.
    assert!(
        implements_targets.is_empty(),
        "ambiguous bare inherent impl mints no IMPLEMENTS edge: {implements_targets:?}"
    );
}

// ---------------------------------------------------------------------------
// Round-8 review: the LOCAL/inline-module analog of the cross-file bare-import
// finding. A generic impl inside an INLINE module implements a `use`-aliased
// non-root trait by bare name while the same file also holds a same-named ROOT
// trait. The generic-impl `Resolve(bare_trait)` path fed the LOCAL per-file
// scope walk, which (not import-aware) walked bare `T` OUTWARD past the absent
// `m::T` and bound the ROOT `T` at depth 0 — a WRONG IMPLEMENTS edge, since the
// `use crate::a::T` alias means `a::T`. Non-root use-aliases are documented out
// of scope, so this must be UNRESOLVED, not a wrong edge. The local resolver now
// mirrors the cross-file same-name ambiguity guard (via a shared predicate).
// ---------------------------------------------------------------------------

#[test]
fn real_scan_local_generic_alias_impl_is_not_misresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    // All three items live in one scanned source file (inline modules), so this
    // exercises the LOCAL per-file resolver, not the cross-file pass.
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {}\n\n",
            "pub mod a {\n",
            "    pub trait T {}\n",
            "}\n\n",
            "pub mod m {\n",
            "    use crate::a::T;\n\n",
            "    pub struct Foo<U>(U);\n\n",
            "    impl<U> T for Foo<U> {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The root trait `T` must gain NO implementor: binding `Foo` to it is the
    // wrong-edge regression (the `use crate::a::T` alias means `a::T`).
    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert!(
        !rows
            .iter()
            .filter_map(|r| r["implementing_type"].as_str())
            .any(|ty| ty == "m::Foo"),
        "root trait T must not gain the use-aliased Foo as an implementor: {rows:?}"
    );

    // Stronger bound: no IMPLEMENTS edge targets the ROOT trait `T`. Locate the
    // root `T` node id and assert no IMPLEMENTS edge points to it.
    let graph = fs::read_to_string(&graph_path).expect("read graph");
    let records: Vec<serde_json::Value> = graph
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let root_t_id = records
        .iter()
        .find(|r| r["record_type"] == "node" && r["symbol_kind"] == "trait" && r["name"] == "T")
        .and_then(|r| r["id"].as_str().map(str::to_owned))
        .expect("root trait T node present");
    let implements_to_root_t = records
        .iter()
        .filter(|r| r["record_type"] == "edge" && r["label"] == "IMPLEMENTS")
        .filter_map(|r| r["target"].as_str())
        .any(|target| target == root_t_id);
    assert!(
        !implements_to_root_t,
        "no IMPLEMENTS edge may target the root trait T: {records:?}"
    );
}

// ---------------------------------------------------------------------------
// The same-name ambiguity guard must NOT over-suppress a legitimate single-name
// outward resolve. A lone trait `T` at the crate root implemented by bare name
// from a nested inline module (`mod m { impl T for X }`, no same-name collision)
// still resolves outward to the root trait — the guard only fires when the bare
// simple name is ambiguous across the file's impl-target definitions.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_local_unambiguous_bare_impl_still_resolves_outward() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait T {}\n\n",
            "pub mod m {\n",
            "    pub struct Bar;\n\n",
            "    impl T for Bar {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "T", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let implementing_types: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["implementing_type"].as_str())
        .collect();
    assert_eq!(
        implementing_types,
        vec!["m::Bar"],
        "unambiguous bare `impl T` in a nested module still resolves outward to the \
         lone root trait T: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Round-9 review: the import-shadow veto closes the general bare-name wrong-edge
// family, not just the SAME-NAME-ambiguous cases the round-8 counting guard saw.
// An EXTERNAL/std trait brought in by `use std::fmt::Display;` is invisible to
// the local impl-target index, so the same-name ambiguity guard sees only ONE
// `Display` (the root) and does NOT fire; the module-scope outward walk then
// binds bare `Display` to the local ROOT trait and mints a WRONG IMPLEMENTS
// edge. The fix records, from the AST, that a `use` visible in the impl's module
// scope ends in the SAME bare segment as the trait name, and leaves the impl
// unresolved regardless of whether the import target is locally known. Correct
// import-aware resolution is follow-up #393.
//
// Local (inline-module) path: `mod m { use std::fmt::Display; impl<T> Display
// for Foo<T> }` alongside a root `pub trait Display {}`.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_local_generic_external_import_is_not_misresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    // All items live in one scanned source file (inline module), so this
    // exercises the LOCAL per-file resolver, not the cross-file pass.
    fs::write(
        src.join("lib.rs"),
        concat!(
            "pub trait Display {}\n\n",
            "pub mod m {\n",
            "    use std::fmt::Display;\n\n",
            "    pub struct Foo<T>(T);\n\n",
            "    impl<T> Display for Foo<T> {}\n",
            "}\n",
        ),
    )
    .expect("write lib.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The root trait `Display` must gain NO implementor: bare `Display` in `m`
    // refers to the `use std::fmt::Display` import, not the local root trait.
    let stdout = egregore()
        .args(["query", "implementors", "Display", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert!(
        !rows
            .iter()
            .filter_map(|r| r["implementing_type"].as_str())
            .any(|ty| ty == "m::Foo"),
        "root trait Display must not gain the std-imported Foo as an implementor: {rows:?}"
    );

    // Stronger bound: no IMPLEMENTS edge targets the ROOT trait `Display`.
    let graph = fs::read_to_string(&graph_path).expect("read graph");
    let records: Vec<serde_json::Value> = graph
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let root_display_id = records
        .iter()
        .find(|r| {
            r["record_type"] == "node" && r["symbol_kind"] == "trait" && r["name"] == "Display"
        })
        .and_then(|r| r["id"].as_str().map(str::to_owned))
        .expect("root trait Display node present");
    let implements_to_root = records
        .iter()
        .filter(|r| r["record_type"] == "edge" && r["label"] == "IMPLEMENTS")
        .filter_map(|r| r["target"].as_str())
        .any(|target| target == root_display_id);
    assert!(
        !implements_to_root,
        "no IMPLEMENTS edge may target the root trait Display: {records:?}"
    );
}

// ---------------------------------------------------------------------------
// Round-9 review: the cross-file analog of the import-shadow veto. An out-of-line
// module (`src/m.rs`) imports an EXTERNAL/std trait by `use std::fmt::Display;`
// and implements it by bare name, while `src/lib.rs` defines a same-named root
// trait. The std import is invisible to the repo impl-target index, so the
// same-name ambiguity guard sees only the root `Display` and does NOT fire; the
// cross-file scope walk then binds bare `Display` to the root trait — a WRONG
// cross-file IMPLEMENTS edge. The `PendingImplFact` now carries an AST-derived
// `shadowed_by_use` boolean set at extraction time, and the cross-file resolver
// vetoes when it is true. Correct import-aware resolution is follow-up #393.
// ---------------------------------------------------------------------------

#[test]
fn real_scan_cross_file_external_import_is_not_misresolved() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        concat!("pub trait Display {}\n\n", "pub mod m;\n"),
    )
    .expect("write lib.rs");
    fs::write(
        src.join("m.rs"),
        concat!(
            "use std::fmt::Display;\n\n",
            "pub struct Foo;\n\n",
            "impl Display for Foo {}\n",
        ),
    )
    .expect("write m.rs");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // The root trait `Display` must gain NO implementor: bare `Display` in m.rs
    // refers to the `use std::fmt::Display` import, not the local root trait.
    let stdout = egregore()
        .args(["query", "implementors", "Display", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert!(
        rows.iter()
            .all(|r| r["implementing_type"] != "m::Foo" && r["implementing_type"] != "Foo"),
        "root trait Display must not gain the std-imported cross-file Foo: {rows:?}"
    );

    // Stronger bound: no cross-file IMPLEMENTS edge targets the ROOT `Display`.
    let graph = fs::read_to_string(&graph_path).expect("read graph");
    let records: Vec<serde_json::Value> = graph
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    let root_display_id = records
        .iter()
        .find(|r| {
            r["record_type"] == "node" && r["symbol_kind"] == "trait" && r["name"] == "Display"
        })
        .and_then(|r| r["id"].as_str().map(str::to_owned))
        .expect("root trait Display node present");
    let implements_to_root = records
        .iter()
        .filter(|r| r["record_type"] == "edge" && r["label"] == "IMPLEMENTS")
        .filter_map(|r| r["target"].as_str())
        .any(|target| target == root_display_id);
    assert!(
        !implements_to_root,
        "no cross-file IMPLEMENTS edge may target the root trait Display: {records:?}"
    );
}

// ---------------------------------------------------------------------------
// Record-ID handles must stay valid under temporal selectors (PR #296 review):
// `--at` / `--as-of` resolution must accept the trait's canonical record ID,
// not only its name.
// ---------------------------------------------------------------------------

#[test]
fn record_id_handle_works_with_temporal_selectors() {
    let (_temp, graph, impl1_id, impl2_id, trait_id) = seed_history();

    // --at by canonical record ID
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            &trait_id,
            "--at",
            "aaaa1111",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "one implementor at c1 by ID: {rows:?}");
    assert_eq!(rows[0]["record_id"], impl1_id.as_str());
    assert_eq!(rows[0]["trait_record_id"], trait_id.as_str());

    // --as-of by canonical record ID
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            &trait_id,
            "--as-of",
            "2026-01-02T12:00:00Z",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 2, "two implementors as of t2 by ID: {rows:?}");
    assert!(
        rows.iter().any(|r| r["record_id"] == impl2_id.as_str()),
        "impl2 must appear as of t2: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// The embedded --data-dir path must be strictly read-only (PR #296 review):
// opening the live engine re-persists index files, so the query must operate
// on a throwaway copy and leave the store byte-for-byte untouched.
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

    egregore()
        .arg("ingest")
        .arg(&f.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = snapshot_tree(&data_dir);
    egregore()
        .args(["query", "implementors", "Renderable", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    egregore()
        .args(["query", "implementors", "Unimplemented", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = snapshot_tree(&data_dir);
    assert_eq!(
        before, after,
        "querying the embedded store must not create, modify, or delete any store file"
    );
}

// ---------------------------------------------------------------------------
// Implementing-type resolution must never cross the repository boundary
// (PR #296 review): repo B's `Foo` must not be attributed to repo A's impl.
// ---------------------------------------------------------------------------

#[test]
fn implementing_type_never_resolves_across_repositories() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("multi_repo.jsonl");
    let mut graph = Graph::new();

    // Repo A: trait CrossT and its impl; the implementing type `Foo` is NOT
    // defined anywhere in repo A.
    let repo_a = stable_id(&["node", "Repository", "repo-a"]);
    graph.push(GraphRecord::node(
        repo_a.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    ));
    let a_path = "src/a_trait.rs";
    let a_file = file_id(a_path);
    graph.push(GraphRecord::syntax_node(
        a_file.clone(),
        NodeKind::File,
        a_path.to_owned(),
        span(1, 60),
        "a_trait.rs".to_owned(),
        "rust",
        format!("Source file {a_path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_a,
        a_file,
        None,
        format!("repo-a contains {a_path}"),
    ));
    let trait_id = push_symbol(&mut graph, a_path, "CrossT", "trait", span(2, 6));
    let impl_id = push_symbol(
        &mut graph,
        a_path,
        "impl CrossT for Foo",
        "impl",
        span(8, 14),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id.clone(),
        trait_id,
        None,
        "impl CrossT for Foo implementation relationship".to_owned(),
    ));

    // Repo B: defines a struct named `Foo` — same name, different repository.
    let repo_b = stable_id(&["node", "Repository", "repo-b"]);
    graph.push(GraphRecord::node(
        repo_b.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-b".to_owned()),
        "Repository repo-b".to_owned(),
    ));
    let b_path = "src/b_types.rs";
    let b_file = file_id(b_path);
    graph.push(GraphRecord::syntax_node(
        b_file.clone(),
        NodeKind::File,
        b_path.to_owned(),
        span(1, 40),
        "b_types.rs".to_owned(),
        "rust",
        format!("Source file {b_path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_b,
        b_file,
        None,
        format!("repo-b contains {b_path}"),
    ));
    let foreign_foo_id = push_symbol(&mut graph, b_path, "Foo", "struct", span(3, 5));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "CrossT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(row["record_id"], impl_id.as_str());
    assert_eq!(
        row["implementing_type"], "Foo",
        "parsed type name is still reported: {row}"
    );
    assert_eq!(
        row["implementing_type_resolution"], "parsed_only",
        "repo B's Foo must not be attributed to repo A's impl: {row}"
    );
    assert!(
        row.get("implementing_type_record_id").is_none()
            || row["implementing_type_record_id"].is_null(),
        "no cross-repository record ID may be cited: {row}"
    );
    assert_ne!(
        row["implementing_type_record_id"],
        foreign_foo_id.as_str(),
        "must never cite the foreign repository's record"
    );
}

// ---------------------------------------------------------------------------
// A tombstoned IMPLEMENTS edge must not resurface in the current view
// (PR #296 review): incremental invalidation tombstones edge record IDs
// themselves when a relationship disappears from a rebuilt file, so an edge
// whose SOURCE symbol is still live can itself be dead.
// ---------------------------------------------------------------------------

#[test]
fn tombstoned_implements_edge_excluded_from_current_view() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tombstoned_edge.jsonl");
    let mut graph = Graph::new();

    let p = "src/gone_impl.rs";
    let trait_id = push_symbol(&mut graph, p, "EdgeGoneTrait", "trait", span(2, 6));
    // The impl symbol survives the rebuild (still live, NOT tombstoned)...
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl EdgeGoneTrait for Survivor",
        "impl",
        span(8, 14),
    );
    push_symbol(&mut graph, p, "Survivor", "struct", span(16, 18));
    // ...but the IMPLEMENTS relationship disappeared: the edge record itself
    // is tombstoned by its own stable ID, as incremental invalidation does.
    let edge = GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl EdgeGoneTrait for Survivor implementation relationship".to_owned(),
    );
    let edge_id = edge.id().to_owned();
    graph.push(edge);
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &edge_id]),
        schema_version: aletheia_egregore::ir::SCHEMA_VERSION,
        deleted_id: edge_id.clone(),
        summary: "IMPLEMENTS relationship removed by rebuild".to_owned(),
        producer: None,
    });

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    // The trait is live but its only IMPLEMENTS edge is tombstoned: the
    // answer must be the explicit zero signal, never the stale implementor.
    let stdout = egregore()
        .args(["query", "implementors", "EdgeGoneTrait", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let lines: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one zero-signal line, no stale implementor rows: {out}"
    );
    assert_eq!(
        lines[0]["code"], "zero_implementors_recorded",
        "tombstoned edge must not resurface as an implementor: {}",
        lines[0]
    );
    assert!(
        out.lines().all(|l| !l.contains(&edge_id)),
        "the tombstoned edge record ID must not be cited: {out}"
    );
}

// ---------------------------------------------------------------------------
// Non-Rust IMPLEMENTS edges carry the implementing type as the edge SOURCE
// (PR #296 review): a Python class inheriting a local base, a TS heritage
// clause, or a Go embedding has no `impl X for Y` display name — the source
// record itself is the citable implementing type and must be cited as such.
// ---------------------------------------------------------------------------

#[test]
fn python_class_inheritance_cites_source_as_implementing_type() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(
        temp.path().join("shapes.py"),
        concat!(
            "class Base:\n",
            "    def area(self):\n",
            "        return 0\n",
            "\n",
            "\n",
            "class Child(Base):\n",
            "    def area(self):\n",
            "        return 1\n",
        ),
    )
    .expect("write shapes.py");

    let graph_path = temp.path().join("graph.jsonl");
    egregore()
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "implementors", "shapes.Base", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert!(
        row["implementing_type"]
            .as_str()
            .is_some_and(|t| t.ends_with("Child")),
        "the inheriting class is the implementing type: {row}"
    );
    assert_eq!(
        row["implementing_type_resolution"], "resolved",
        "the source record IS the implementing type — it must be cited, \
         not degraded to parsed_only: {row}"
    );
    assert_eq!(
        row["implementing_type_record_id"], row["record_id"],
        "the citable type record is the edge source itself: {row}"
    );
}

// ---------------------------------------------------------------------------
// Type-defining symbols must outrank same-file non-types in implementing-type
// resolution (PR #296 review): a same-file function named `Foo` must not beat
// the real `struct Foo` defined in another file.
// ---------------------------------------------------------------------------

#[test]
fn type_symbol_outranks_same_file_non_type() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ranking.jsonl");
    let mut graph = Graph::new();

    let a_path = "src/rank_a.rs";
    let b_path = "src/rank_b.rs";
    let trait_id = push_symbol(&mut graph, a_path, "RankT", "trait", span(2, 5));
    let impl_id = push_symbol(
        &mut graph,
        a_path,
        "impl RankT for Foo",
        "impl",
        span(7, 12),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl RankT for Foo implementation relationship".to_owned(),
    ));
    // Decoy: a same-file NON-type symbol named Foo (a function)...
    let decoy_fn_id = push_symbol(&mut graph, a_path, "Foo", "function", span(14, 18));
    // ...while the real struct Foo lives in another file.
    let struct_id = push_symbol(&mut graph, b_path, "Foo", "struct", span(3, 5));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "RankT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(row["implementing_type_resolution"], "resolved", "{row}");
    assert_eq!(
        row["implementing_type_record_id"],
        struct_id.as_str(),
        "the type-defining struct must outrank the same-file function decoy \
         ({decoy_fn_id}): {row}"
    );
}

// ---------------------------------------------------------------------------
// --as-of must return ALL same-named trait candidates at the instant
// (PR #296 review): the ambiguity rule (every candidate, labeled by
// trait_record_id) applies to temporal views too — best-per-repository
// selection must not silently pick one of two same-repo traits.
// ---------------------------------------------------------------------------

#[test]
fn as_of_ambiguous_name_returns_all_candidates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("as_of_ambiguous.jsonl");
    let mut graph = Graph::new();
    let c1 = "cccc333333333333333333333333333333333333";
    let t1 = "2026-01-01T00:00:00Z";

    // Two distinct traits named DupT in different files, each with one
    // implementor, all recorded at the same commit/instant.
    for (p, ty) in [("src/dup_a.rs", "AlphaTy"), ("src/dup_b.rs", "BetaTy")] {
        let trait_id = sym_id(p, "DupT");
        let impl_id = sym_id(p, &format!("impl DupT for {ty}"));
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "DupT".to_owned(),
                "rust",
                0,
                "Rust trait DupT".to_owned(),
            )
            .with_temporal(temporal(c1, t1)),
        );
        graph.push(
            GraphRecord::syntax_symbol(
                impl_id.clone(),
                "impl",
                p.to_owned(),
                span(6, 9),
                format!("impl DupT for {ty}"),
                "rust",
                0,
                format!("Rust impl impl DupT for {ty}"),
            )
            .with_temporal(temporal(c1, t1)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl_id,
                trait_id,
                None,
                format!("impl DupT for {ty} implementation relationship"),
            )
            .with_temporal(temporal(c1, t1)),
        );
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "DupT",
            "--as-of",
            "2026-01-01T12:00:00Z",
            "--graph",
        ])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "both same-named trait candidates must be reported under --as-of: {rows:?}"
    );
    let trait_ids: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r["trait_record_id"].as_str())
        .collect();
    assert_eq!(
        trait_ids.len(),
        2,
        "rows must be labeled by both distinct trait_record_ids: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// --data-dir plus a temporal selector must read the history-inclusive store
// view (PR #296 review): the embedded store keeps older commit versions only
// in the history view, so a current-view read wrongly loses pinned answers.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_at_commit_returns_pinned_implementors() {
    let (_temp, graph, impl1_id, impl2_id, _trait_id) = seed_history();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let at = |commit: &str| -> Vec<serde_json::Value> {
        let stdout = egregore()
            .args([
                "query",
                "implementors",
                "Pinned",
                "--at",
                commit,
                "--data-dir",
            ])
            .arg(&data_dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    };

    let rows_c1 = at("aaaa1111");
    assert_eq!(
        rows_c1.len(),
        1,
        "the older commit's implementor set must survive embedded ingest: {rows_c1:?}"
    );
    assert_eq!(rows_c1[0]["record_id"], impl1_id.as_str());

    let rows_c2 = at("bbbb2222");
    assert_eq!(rows_c2.len(), 2, "two implementors at c2: {rows_c2:?}");
    assert!(
        rows_c2.iter().any(|r| r["record_id"] == impl2_id.as_str()),
        "impl2 must appear at c2: {rows_c2:?}"
    );
}

// ---------------------------------------------------------------------------
// Multi-repository current view (PR #296 review): the unscoped current view
// is a LIST query — colliding same-named traits from different repositories
// are returned side by side, each row labeled by its own repository_id and
// trait_record_id, never merged and never silently reduced. --repo narrows
// to one repository. (Fail-closed is reserved for single-answer time views,
// per the repository-scope contract in docs/cli/query.md.)
// ---------------------------------------------------------------------------

fn seed_two_repo_shared_trait() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("two_repo.jsonl");
    let mut graph = Graph::new();

    let mut trait_ids = Vec::new();
    for (repo_name, p) in [("repo-a", "src/shared_a.rs"), ("repo-b", "src/shared_b.rs")] {
        let repo_id = stable_id(&["node", "Repository", repo_name]);
        graph.push(GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(repo_name.to_owned()),
            format!("Repository {repo_name}"),
        ));
        let fid = file_id(p);
        graph.push(GraphRecord::syntax_node(
            fid.clone(),
            NodeKind::File,
            p.to_owned(),
            span(1, 60),
            p.rsplit('/').next().unwrap_or(p).to_owned(),
            "rust",
            format!("Source file {p}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id,
            fid,
            None,
            format!("{repo_name} contains {p}"),
        ));
        let trait_id = push_symbol(&mut graph, p, "SharedT", "trait", span(2, 5));
        push_symbol(&mut graph, p, "Thing", "struct", span(7, 9));
        let impl_id = push_symbol(
            &mut graph,
            p,
            "impl SharedT for Thing",
            "impl",
            span(11, 15),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Implements,
            impl_id,
            trait_id.clone(),
            None,
            "impl SharedT for Thing implementation relationship".to_owned(),
        ));
        trait_ids.push(trait_id);
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    let trait_b = trait_ids.pop().expect("trait b");
    let trait_a = trait_ids.pop().expect("trait a");
    (temp, path, trait_a, trait_b)
}

#[test]
fn unscoped_multi_repo_current_view_lists_labeled_candidates() {
    let (_temp, path, trait_a, trait_b) = seed_two_repo_shared_trait();

    let rows = run_rows(&path, "SharedT");
    assert_eq!(
        rows.len(),
        2,
        "one labeled row per repository's trait — side by side, never merged: {rows:?}"
    );
    let repo_ids: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r["repository_id"].as_str())
        .collect();
    assert_eq!(
        repo_ids.len(),
        2,
        "each row must carry its OWN repository_id: {rows:?}"
    );
    let trait_ids: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r["trait_record_id"].as_str())
        .collect();
    assert!(
        trait_ids.contains(trait_a.as_str()) && trait_ids.contains(trait_b.as_str()),
        "rows must be labeled by both distinct trait_record_ids: {rows:?}"
    );
}

#[test]
fn multi_repo_current_view_repo_scope_narrows() {
    let (_temp, path, trait_a, trait_b) = seed_two_repo_shared_trait();

    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "SharedT",
            "--repo",
            "repo-a",
            "--graph",
        ])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "--repo narrows to one repository: {rows:?}");
    assert_eq!(rows[0]["trait_record_id"], trait_a.as_str());
    assert_ne!(rows[0]["trait_record_id"], trait_b.as_str());
}

// ---------------------------------------------------------------------------
// Pinned type resolution must stay at the pinned commit (PR #296 review):
// an impl recorded at commit A whose implementing type gains a local Symbol
// only at commit B must be parsed_only under --at A — citing the future
// record would fabricate history.
// ---------------------------------------------------------------------------

#[test]
fn pinned_type_resolution_never_cites_future_symbols() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("future_type.jsonl");
    let mut graph = Graph::new();
    let ca = "dddd444444444444444444444444444444444444";
    let cb = "eeee555555555555555555555555555555555555";
    let ta = "2026-02-01T00:00:00Z";
    let tb = "2026-02-02T00:00:00Z";
    let p = "src/pin_types.rs";

    let trait_id = sym_id(p, "PinT");
    let impl_id = sym_id(p, "impl PinT for Foo");
    let foo_id = sym_id(p, "Foo");

    // Trait, impl, and edge exist at BOTH commits.
    for (commit, vt) in [(ca, ta), (cb, tb)] {
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "PinT".to_owned(),
                "rust",
                0,
                "Rust trait PinT".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::syntax_symbol(
                impl_id.clone(),
                "impl",
                p.to_owned(),
                span(6, 9),
                "impl PinT for Foo".to_owned(),
                "rust",
                0,
                "Rust impl impl PinT for Foo".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl_id.clone(),
                trait_id.clone(),
                None,
                "impl PinT for Foo implementation relationship".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
    }
    // The local `Foo` struct exists ONLY at the later commit B.
    graph.push(
        GraphRecord::syntax_symbol(
            foo_id.clone(),
            "struct",
            p.to_owned(),
            span(11, 13),
            "Foo".to_owned(),
            "rust",
            0,
            "Rust struct Foo".to_owned(),
        )
        .with_temporal(temporal(cb, tb)),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let at = |commit: &str| -> Vec<serde_json::Value> {
        let stdout = egregore()
            .args(["query", "implementors", "PinT", "--at", commit, "--graph"])
            .arg(&path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    };

    // --at A: Foo has no record yet — parsed_only, no future record cited.
    let rows_a = at("dddd4444");
    assert_eq!(rows_a.len(), 1, "one implementor at commit A: {rows_a:?}");
    assert_eq!(
        rows_a[0]["implementing_type_resolution"], "parsed_only",
        "a type introduced only at a later commit must not resolve at an \
         earlier pin: {rows_a:?}"
    );
    assert!(
        rows_a[0].get("implementing_type_record_id").is_none()
            || rows_a[0]["implementing_type_record_id"].is_null(),
        "no future record ID may be cited at commit A: {rows_a:?}"
    );

    // --at B: Foo now exists — resolved, citing its record.
    let rows_b = at("eeee5555");
    assert_eq!(rows_b.len(), 1, "one implementor at commit B: {rows_b:?}");
    assert_eq!(rows_b[0]["implementing_type_resolution"], "resolved");
    assert_eq!(rows_b[0]["implementing_type_record_id"], foo_id.as_str());
}

// ---------------------------------------------------------------------------
// Generic impl headers must never cite the impl symbol as the implementing
// type (PR #296 review): `impl<T> Trait for Wrapper<T>` has no `"impl "`
// substring, so the old parser returned None and the type-source fallback
// wrongly cited the impl record itself. The generic header must parse to
// `Wrapper` (resolved against the graph) — and an impl-kind source must
// never count as a type-source.
// ---------------------------------------------------------------------------

#[test]
fn generic_impl_never_cites_impl_record_as_type() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("generic_impl.jsonl");
    let mut graph = Graph::new();

    let p = "src/generic.rs";
    let trait_id = push_symbol(&mut graph, p, "GenT", "trait", span(2, 5));
    let wrapper_id = push_symbol(&mut graph, p, "Wrapper", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl<T> GenT for Wrapper<T>",
        "impl",
        span(11, 16),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id.clone(),
        trait_id,
        None,
        "impl<T> GenT for Wrapper<T> implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "GenT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_ne!(
        row["implementing_type_record_id"],
        impl_id.as_str(),
        "the impl symbol must never be cited as the implementing type: {row}"
    );
    assert_eq!(
        row["implementing_type"], "Wrapper",
        "the generic header parses to the base implementing type: {row}"
    );
    assert_eq!(row["implementing_type_resolution"], "resolved", "{row}");
    assert_eq!(
        row["implementing_type_record_id"],
        wrapper_id.as_str(),
        "the Wrapper struct record is the citable type: {row}"
    );
}

#[test]
fn generic_impl_without_local_type_is_parsed_only() {
    // Same generic header, but no local `Wrapper` Symbol exists: the row must
    // degrade to parsed_only — never cite the impl record as the type.
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("generic_impl_unresolved.jsonl");
    let mut graph = Graph::new();

    let p = "src/generic_unresolved.rs";
    let trait_id = push_symbol(&mut graph, p, "GenU", "trait", span(2, 5));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl<T: Into<String>> GenU for Missing<T>",
        "impl",
        span(7, 12),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id.clone(),
        trait_id,
        None,
        "impl<T: Into<String>> GenU for Missing<T> implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "GenU");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_ne!(
        row["implementing_type_record_id"],
        impl_id.as_str(),
        "the impl symbol must never be cited as the implementing type: {row}"
    );
    assert_eq!(
        row["implementing_type_resolution"], "parsed_only",
        "no local type record — parsed_only, never a fabricated handle: {row}"
    );
    assert_eq!(
        row["implementing_type"], "Missing",
        "nested generics in the parameter list must not derail parsing: {row}"
    );
}

// ---------------------------------------------------------------------------
// Scan-history frontier semantics (PR #296 review): locked as the established
// repo-wide convention, not changed.
//
// scan-history replays full per-commit snapshots and emits NO tombstone when
// a symbol or relationship disappears (removal is visible as absence from
// later snapshots). The shipped sibling verbs treat an UNPINNED query over
// such a store as the union across recorded history — `eg query symbol`
// lists every snapshot version, change-impact applies no temporal filter,
// and transitive-callers explicitly dedups "the same stable edge ID
// recur[ring] across commit snapshots". The frontier answer is obtained by
// pinning: `--at <tip>`. Current-tree stores (eg scan / incremental) carry
// real tombstones and are already frontier-accurate (see the tombstoned-edge
// and stale-handle tests above).
// ---------------------------------------------------------------------------

/// History fixture: `FrontT` has two implementors at c1; the second one is
/// removed at c2 (the tip). `VanishedT` and its single implementor exist
/// only at c1 — gone from later snapshots without any tombstone.
#[allow(clippy::type_complexity)]
fn seed_frontier_history() -> (tempfile::TempDir, PathBuf, String, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("frontier.jsonl");
    let mut graph = Graph::new();
    let c1 = "ffff666666666666666666666666666666666666";
    let c2 = "abab777777777777777777777777777777777777";
    let t1 = "2026-03-01T00:00:00Z";
    let t2 = "2026-03-02T00:00:00Z";
    let p = "src/frontier.rs";

    let front_trait = sym_id(p, "FrontT");
    let keep_impl = sym_id(p, "impl FrontT for Kept");
    let gone_impl = sym_id(p, "impl FrontT for Removed");
    let vanished_trait = sym_id(p, "VanishedT");
    let vanished_impl = sym_id(p, "impl VanishedT for Old");

    let mut push_snap = |id: &str, kind: &str, name: &str, sp: SourceSpan, c: &str, vt: &str| {
        graph.push(
            GraphRecord::syntax_symbol(
                id.to_owned(),
                kind,
                p.to_owned(),
                sp,
                name.to_owned(),
                "rust",
                0,
                format!("Rust {kind} {name}"),
            )
            .with_temporal(temporal(c, vt)),
        );
    };

    for (c, vt) in [(c1, t1), (c2, t2)] {
        push_snap(&front_trait, "trait", "FrontT", span(1, 4), c, vt);
        push_snap(
            &keep_impl,
            "impl",
            "impl FrontT for Kept",
            span(6, 9),
            c,
            vt,
        );
    }
    // Removed-at-tip implementor and the vanished trait: c1 snapshots only.
    push_snap(
        &gone_impl,
        "impl",
        "impl FrontT for Removed",
        span(11, 14),
        c1,
        t1,
    );
    push_snap(&vanished_trait, "trait", "VanishedT", span(16, 19), c1, t1);
    push_snap(
        &vanished_impl,
        "impl",
        "impl VanishedT for Old",
        span(21, 24),
        c1,
        t1,
    );

    for (c, vt) in [(c1, t1), (c2, t2)] {
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                keep_impl.clone(),
                front_trait.clone(),
                None,
                "impl FrontT for Kept implementation relationship".to_owned(),
            )
            .with_temporal(temporal(c, vt)),
        );
    }
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            gone_impl.clone(),
            front_trait.clone(),
            None,
            "impl FrontT for Removed implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c1, t1)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            vanished_impl.clone(),
            vanished_trait.clone(),
            None,
            "impl VanishedT for Old implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c1, t1)),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path, keep_impl, gone_impl, vanished_impl)
}

#[test]
fn unpinned_history_view_is_union_and_tip_pin_is_frontier() {
    let (_temp, path, keep_impl, gone_impl, _vanished) = seed_frontier_history();

    // Unpinned over a scan-history store: the union across recorded history,
    // every row labeled with its provenance commit — matching the sibling
    // verbs' convention (symbol lists all versions; transitive-callers walks
    // deduped history edges). Never silently frontier-trimmed.
    let rows = run_rows(&path, "FrontT");
    assert_eq!(rows.len(), 2, "union across history: {rows:?}");
    for row in &rows {
        assert!(
            row["git_commit"].as_str().is_some(),
            "history rows must carry their provenance commit: {row}"
        );
    }
    let ids: std::collections::BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert!(ids.contains(keep_impl.as_str()) && ids.contains(gone_impl.as_str()));

    // The frontier/current answer is the tip pin: the removed implementor is
    // absent from the c2 snapshot.
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "FrontT",
            "--at",
            "abab7777",
            "--graph",
        ])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tip_rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        tip_rows.len(),
        1,
        "tip pin excludes the removed impl: {tip_rows:?}"
    );
    assert_eq!(tip_rows[0]["record_id"], keep_impl.as_str());
}

#[test]
fn as_of_after_disappearance_matches_symbol_verb_parity() {
    let (_temp, path, _keep, _gone, vanished_impl) = seed_frontier_history();
    let instant = "2026-03-03T00:00:00Z"; // after the c2 snapshot

    // Sibling contract first: the shipped `eg query symbol --as-of` resolves
    // a symbol that stopped appearing (no tombstone) at its newest snapshot
    // at or before the instant — timestamp-based, not frontier-based.
    let stdout = egregore()
        .args([
            "query",
            "symbol",
            "VanishedT",
            "--as-of",
            instant,
            "--graph",
        ])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sym_row: serde_json::Value = serde_json::from_str(
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .next()
            .expect("row"),
    )
    .expect("valid JSON");
    assert!(
        sym_row["git_commit"]
            .as_str()
            .is_some_and(|c| c.starts_with("ffff")),
        "sibling symbol verb resolves the c1 snapshot: {sym_row}"
    );

    // Implementors --as-of resolves the SAME snapshot (issue #133 AC:
    // temporal selectors at parity with the symbol verb) and pins the
    // implementor set to it — every row visibly labeled with that commit,
    // never presented as the current state.
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "VanishedT",
            "--as-of",
            instant,
            "--graph",
        ])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "the c1-pinned implementor set: {rows:?}");
    assert_eq!(rows[0]["record_id"], vanished_impl.as_str());
    assert!(
        rows[0]["git_commit"]
            .as_str()
            .is_some_and(|c| c.starts_with("ffff")),
        "rows carry the resolved snapshot's commit: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Qualified implementing-type paths (PR #296 review): a foreign-qualified
// path (`external::Foo`) must never bare-suffix-match a local `Foo` — that
// would cite an unrelated local record for an external type. Only the known
// local qualifiers (`crate::` / `self::`) relax to local path matching.
// ---------------------------------------------------------------------------

#[test]
fn foreign_qualified_type_never_cites_local_suffix_match() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("foreign_qualified.jsonl");
    let mut graph = Graph::new();

    let p = "src/foreign.rs";
    let trait_id = push_symbol(&mut graph, p, "ExtT", "trait", span(2, 5));
    // A local struct that merely SHARES the trailing segment.
    let local_foo_id = push_symbol(&mut graph, p, "Foo", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl ExtT for external::Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl ExtT for external::Foo implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "ExtT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row["implementing_type"], "external::Foo",
        "the qualified path is reported as parsed: {row}"
    );
    assert_eq!(
        row["implementing_type_resolution"], "parsed_only",
        "a foreign-qualified type must not resolve to a local same-suffix \
         record: {row}"
    );
    assert_ne!(
        row["implementing_type_record_id"],
        local_foo_id.as_str(),
        "the unrelated local Foo must never be cited: {row}"
    );
}

#[test]
fn crate_qualified_local_type_still_resolves() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("crate_qualified.jsonl");
    let mut graph = Graph::new();

    let p = "src/crate_local.rs";
    let trait_id = push_symbol(&mut graph, p, "LocalQ", "trait", span(2, 5));
    let bar_id = push_symbol(&mut graph, p, "Bar", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl LocalQ for crate::Bar",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl LocalQ for crate::Bar implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "LocalQ");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row["implementing_type_resolution"], "resolved",
        "crate:: is a known local qualifier — the local Bar resolves: {row}"
    );
    assert_eq!(row["implementing_type_record_id"], bar_id.as_str(), "{row}");
}

// ---------------------------------------------------------------------------
// Where clauses in impl headers (PR #296 review): the extractor's display
// keeps everything before `{`, so `impl T for Circle where Circle: Clone`
// carries the where clause in its name; the parser must strip it so the
// resolvable `Circle` row is not degraded to parsed_only.
// ---------------------------------------------------------------------------

#[test]
fn where_clause_impl_resolves_implementing_type() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("where_clause.jsonl");
    let mut graph = Graph::new();

    let p = "src/where_clause.rs";
    let trait_id = push_symbol(&mut graph, p, "WhereT", "trait", span(2, 5));
    let circle_id = push_symbol(&mut graph, p, "Circle", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl WhereT for Circle where Circle: Clone",
        "impl",
        span(11, 16),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl WhereT for Circle where Circle: Clone implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "WhereT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row["implementing_type"], "Circle",
        "the trailing where clause must be stripped before resolution: {row}"
    );
    assert_eq!(row["implementing_type_resolution"], "resolved", "{row}");
    assert_eq!(
        row["implementing_type_record_id"],
        circle_id.as_str(),
        "{row}"
    );
}

// ---------------------------------------------------------------------------
// Reference-typed impl targets (PR #296 review): `impl T for &mut Foo` and
// `impl T for &'a Foo` must strip the full reference prefix (`&`, optional
// lifetime, optional `mut`) before resolution — not just the leading `&`.
// ---------------------------------------------------------------------------

#[test]
fn reference_impl_targets_resolve() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ref_targets.jsonl");
    let mut graph = Graph::new();

    let p = "src/ref_targets.rs";
    let trait_id = push_symbol(&mut graph, p, "RefT", "trait", span(2, 5));
    let foo_id = push_symbol(&mut graph, p, "Foo", "struct", span(7, 9));
    for (name, sp) in [
        ("impl RefT for &mut Foo", span(11, 14)),
        ("impl RefT for &'a Foo", span(16, 19)),
    ] {
        let impl_id = push_symbol(&mut graph, p, name, "impl", sp);
        graph.push(GraphRecord::edge(
            EdgeLabel::Implements,
            impl_id,
            trait_id.clone(),
            None,
            format!("{name} implementation relationship"),
        ));
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "RefT");
    assert_eq!(rows.len(), 2, "two recorded implementors: {rows:?}");
    for row in &rows {
        assert_eq!(
            row["implementing_type"], "Foo",
            "reference sigils, lifetimes, and mut must be stripped: {row}"
        );
        assert_eq!(
            row["implementing_type_resolution"], "resolved",
            "the referenced type resolves to its record: {row}"
        );
        assert_eq!(row["implementing_type_record_id"], foo_id.as_str(), "{row}");
    }
}

// ---------------------------------------------------------------------------
// self-qualified impl targets (PR #296 review): `self::Foo` names the type
// in the impl's OWN module, which is recoverable from the impl symbol's
// qualified display name ("outer::inner::impl T for self::Foo"). A
// same-named type in a parent/sibling module must not be cited via suffix
// matching or same-file preference.
// ---------------------------------------------------------------------------

#[test]
fn self_qualified_type_resolves_in_impl_module() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("self_qualified.jsonl");
    let mut graph = Graph::new();

    // File A holds the nested-module impl AND the parent module's decoy Foo
    // (same file, so the old same-file preference would pick the decoy).
    let a = "src/self_a.rs";
    let b = "src/self_b.rs";
    let trait_id = push_symbol(&mut graph, a, "outer::inner::SelfT", "trait", span(2, 5));
    let decoy_parent_foo = push_symbol(&mut graph, a, "outer::Foo", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        a,
        "outer::inner::impl SelfT for self::Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "outer::inner::impl SelfT for self::Foo implementation relationship".to_owned(),
    ));
    // The type self:: actually names lives in the impl's own module, defined
    // in another file.
    let inner_foo = push_symbol(&mut graph, b, "outer::inner::Foo", "struct", span(3, 5));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "outer::inner::SelfT");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_eq!(
        row["implementing_type_resolution"], "resolved",
        "self:: resolves within the impl's module: {row}"
    );
    assert_eq!(
        row["implementing_type"], "outer::inner::Foo",
        "self::Foo names the impl module's Foo: {row}"
    );
    assert_eq!(
        row["implementing_type_record_id"],
        inner_foo.as_str(),
        "the parent-module decoy ({decoy_parent_foo}) must not be cited: {row}"
    );
}

// ---------------------------------------------------------------------------
// crate:: paths are absolute (PR #296 review): `crate::a::Foo` names exactly
// the crate-root path `a::Foo` — a same-file decoy `b::a::Foo` sharing the
// trailing path must never be cited via suffix matching.
// ---------------------------------------------------------------------------

#[test]
fn crate_qualified_path_is_absolute() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("crate_absolute.jsonl");
    let mut graph = Graph::new();

    let a = "src/crate_abs_a.rs";
    let b = "src/crate_abs_b.rs";
    let trait_id = push_symbol(&mut graph, a, "CrateAbs", "trait", span(2, 5));
    // Same-file decoy whose path merely ENDS with ::a::Foo.
    let decoy_id = push_symbol(&mut graph, a, "b::a::Foo", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        a,
        "impl CrateAbs for crate::a::Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "impl CrateAbs for crate::a::Foo implementation relationship".to_owned(),
    ));
    // The absolute crate-root item lives in another file.
    let root_foo_id = push_symbol(&mut graph, b, "a::Foo", "struct", span(3, 5));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "CrateAbs");
    assert_eq!(rows.len(), 1, "one recorded implementor: {rows:?}");
    let row = &rows[0];
    assert_ne!(
        row["implementing_type_record_id"],
        decoy_id.as_str(),
        "crate:: is absolute — the same-file b::a::Foo decoy must never be \
         cited: {row}"
    );
    assert_eq!(row["implementing_type_resolution"], "resolved", "{row}");
    assert_eq!(
        row["implementing_type_record_id"],
        root_foo_id.as_str(),
        "the crate-root a::Foo is the exact match: {row}"
    );
}

// ---------------------------------------------------------------------------
// Pinned provenance labels on embedded stores (PR #296 review): when the
// exact edge version was collapsed away and the impl-block fallback supplies
// the row, its git_commit must be the PINNED commit — not the collapsed
// edge's later commit.
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_pinned_rows_labeled_with_pinned_commit() {
    let (_temp, graph, impl1_id, _impl2_id, _trait_id) = seed_history();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    // The impl1 edge exists at c1 and c2 in the JSONL, but the embedded
    // store keeps only the latest physical edge version (c2). Pinning to c1
    // uses the impl-block fallback — and the row must still be labeled with
    // the queried commit, never the collapsed edge's later commit.
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "Pinned",
            "--at",
            "aaaa1111",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(rows.len(), 1, "one implementor at c1: {rows:?}");
    assert_eq!(rows[0]["record_id"], impl1_id.as_str());
    assert!(
        rows[0]["git_commit"]
            .as_str()
            .is_some_and(|c| c.starts_with("aaaa")),
        "pinned rows carry the queried commit as provenance: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Identical as-of timestamps (PR #296 review): scan-history can emit two
// commits with the same committer timestamp. The as-of pin must take the
// LATEST snapshot at the tie (scan-history emits oldest-to-newest), not the
// older one — otherwise the later commit's changes are silently omitted.
// (The symbol verb's cross-ID tie-break is unaffected: this is the per-ID
// snapshot choice that drives the pinned row set.)
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn as_of_identical_timestamps_pin_latest_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tie_ts.jsonl");
    let mut graph = Graph::new();
    let c1 = "1a1a888888888888888888888888888888888888";
    let c2 = "2b2b999999999999999999999999999999999999";
    let vt = "2026-04-01T00:00:00Z"; // identical committer timestamp
    let p = "src/tie.rs";

    let trait_id = sym_id(p, "TieT");
    let impl1_id = sym_id(p, "impl TieT for One");
    let impl2_id = sym_id(p, "impl TieT for Two");

    // Oldest-to-newest emission order: c1 snapshots first, then c2.
    for commit in [c1, c2] {
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "TieT".to_owned(),
                "rust",
                0,
                "Rust trait TieT".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::syntax_symbol(
                impl1_id.clone(),
                "impl",
                p.to_owned(),
                span(6, 9),
                "impl TieT for One".to_owned(),
                "rust",
                0,
                "Rust impl impl TieT for One".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl1_id.clone(),
                trait_id.clone(),
                None,
                "impl TieT for One implementation relationship".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
    }
    // Second implementor exists only in the LATER commit's snapshot.
    graph.push(
        GraphRecord::syntax_symbol(
            impl2_id.clone(),
            "impl",
            p.to_owned(),
            span(11, 14),
            "impl TieT for Two".to_owned(),
            "rust",
            0,
            "Rust impl impl TieT for Two".to_owned(),
        )
        .with_temporal(temporal(c2, vt)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            impl2_id.clone(),
            trait_id.clone(),
            None,
            "impl TieT for Two implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c2, vt)),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let run_as_of = |handle: &str| -> Vec<serde_json::Value> {
        let stdout = egregore()
            .args(["query", "implementors", handle, "--as-of", vt, "--graph"])
            .arg(&path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    };

    // By name and by canonical record ID: both must pin the later snapshot.
    for handle in ["TieT", trait_id.as_str()] {
        let rows = run_as_of(handle);
        assert_eq!(
            rows.len(),
            2,
            "handle {handle}: the later commit's snapshot wins the timestamp \
             tie — both implementors must appear: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r["record_id"] == impl2_id.as_str()),
            "handle {handle}: impl2 (later commit only) must appear: {rows:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Non-target symbol kinds (PR #296 review): a `fn Foo` sharing the trait's
// name lives in a different namespace and can never be an IMPLEMENTS target;
// it must not produce a spurious zero_implementors_recorded row next to the
// trait's real rows. An explicit record-ID handle still answers for exactly
// that record.
// ---------------------------------------------------------------------------

#[test]
fn non_target_kinds_excluded_from_name_resolution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("non_target.jsonl");
    let mut graph = Graph::new();

    let p = "src/non_target.rs";
    let trait_id = push_symbol(&mut graph, p, "Shape", "trait", span(2, 5));
    push_symbol(&mut graph, p, "Circle", "struct", span(7, 9));
    let impl_id = push_symbol(&mut graph, p, "impl Shape for Circle", "impl", span(11, 15));
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id.clone(),
        trait_id,
        None,
        "impl Shape for Circle implementation relationship".to_owned(),
    ));
    // A function in the value namespace sharing the trait's name.
    let fn_id = push_symbol(&mut graph, "src/other.rs", "Shape", "function", span(3, 6));
    // A name that resolves ONLY to a function.
    push_symbol(&mut graph, p, "compute", "function", span(17, 20));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    // Name lookup: only the trait candidate answers — exactly one row, no
    // spurious zero signal for the function.
    let rows = run_rows(&path, "Shape");
    assert_eq!(
        rows.len(),
        1,
        "the function namesake must not add a zero_implementors_recorded \
         row: {rows:?}"
    );
    assert_eq!(rows[0]["record_id"], impl_id.as_str());
    assert!(
        rows[0].get("code").is_none(),
        "the single line is a real implementor row: {rows:?}"
    );

    // A name resolving ONLY to a non-target kind is a no_match with a clear
    // message — never a zero signal that mislabels a function as a trait.
    let stdout = egregore()
        .args(["query", "implementors", "compute", "--graph"])
        .arg(&path)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match", "got: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("function")),
        "the diagnostic names the non-target kind: {v}"
    );

    // An explicit record-ID handle still answers for exactly that record.
    let stdout = egregore()
        .args(["query", "implementors", &fn_id, "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let line: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one line")).expect("valid JSON");
    assert_eq!(
        line["code"], "zero_implementors_recorded",
        "record-ID handles answer for the precise record: {line}"
    );
}

// ---------------------------------------------------------------------------
// Chronological valid_time comparison (PR #296 review): scan-history stores
// the committer's LOCAL offset (`git show --format=%cI`; only +00:00 is
// rewritten to Z), so RFC 3339 strings do not sort chronologically across
// offsets. The unpinned newest-version choice must compare instants, not
// bytes: `2026-01-01T23:30:00-02:00` (= 01:30Z on Jan 2) is LATER than
// `2026-01-02T00:15:00Z` despite comparing smaller lexically.
// ---------------------------------------------------------------------------

#[test]
fn unpinned_view_compares_valid_time_chronologically() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("offsets.jsonl");
    let mut graph = Graph::new();
    let c_old = "3c3caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let c_new = "4d4dbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let vt_old = "2026-01-02T00:15:00Z"; // lexically LARGER
    let vt_new = "2026-01-01T23:30:00-02:00"; // chronologically LATER (01:30Z Jan 2)
    let p = "src/offsets.rs";

    let trait_id = sym_id(p, "OffsetT");
    let impl_id = sym_id(p, "impl OffsetT for Zoned");

    for (commit, vt) in [(c_old, vt_old), (c_new, vt_new)] {
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "OffsetT".to_owned(),
                "rust",
                0,
                "Rust trait OffsetT".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::syntax_symbol(
                impl_id.clone(),
                "impl",
                p.to_owned(),
                span(6, 9),
                "impl OffsetT for Zoned".to_owned(),
                "rust",
                0,
                "Rust impl impl OffsetT for Zoned".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl_id.clone(),
                trait_id.clone(),
                None,
                "impl OffsetT for Zoned implementation relationship".to_owned(),
            )
            .with_temporal(temporal(commit, vt)),
        );
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "OffsetT");
    assert_eq!(rows.len(), 1, "one implementor: {rows:?}");
    assert!(
        rows[0]["git_commit"]
            .as_str()
            .is_some_and(|c| c.starts_with("4d4d")),
        "the unpinned view must pick the chronologically newest snapshot \
         (instant comparison), not the lexically largest string: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// super-qualified impl targets (PR #296 review): `outer::inner::impl T for
// super::Foo` names the PARENT module's Foo, recoverable by walking up the
// impl symbol's module prefix. Repeated `super::` walks further up; a
// top-level impl using `super::` (invalid Rust) degrades honestly.
// ---------------------------------------------------------------------------

#[test]
fn super_qualified_type_resolves_in_parent_module() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("super_qualified.jsonl");
    let mut graph = Graph::new();

    let a = "src/super_a.rs";
    let b = "src/super_b.rs";

    // super:: — one level up.
    let sup_trait = push_symbol(&mut graph, a, "outer::inner::SupT", "trait", span(2, 5));
    let sup_impl = push_symbol(
        &mut graph,
        a,
        "outer::inner::impl SupT for super::Foo",
        "impl",
        span(7, 11),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        sup_impl,
        sup_trait,
        None,
        "outer::inner::impl SupT for super::Foo implementation relationship".to_owned(),
    ));
    let parent_foo = push_symbol(&mut graph, b, "outer::Foo", "struct", span(3, 5));

    // super::super:: — two levels up.
    let sup2_trait = push_symbol(&mut graph, a, "a::b::c::SupT2", "trait", span(13, 16));
    let sup2_impl = push_symbol(
        &mut graph,
        a,
        "a::b::c::impl SupT2 for super::super::Widget",
        "impl",
        span(18, 22),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        sup2_impl,
        sup2_trait,
        None,
        "a::b::c::impl SupT2 for super::super::Widget implementation relationship".to_owned(),
    ));
    let a_widget = push_symbol(&mut graph, b, "a::Widget", "struct", span(7, 9));

    // Top-level impl using super:: — not derivable, must degrade.
    let top_trait = push_symbol(&mut graph, a, "SupT3", "trait", span(24, 27));
    let top_impl = push_symbol(
        &mut graph,
        a,
        "impl SupT3 for super::Ghost",
        "impl",
        span(29, 33),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        top_impl,
        top_trait,
        None,
        "impl SupT3 for super::Ghost implementation relationship".to_owned(),
    ));
    push_symbol(&mut graph, b, "Ghost", "struct", span(11, 13));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "outer::inner::SupT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_resolution"], "resolved",
        "{rows:?}"
    );
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        parent_foo.as_str(),
        "super::Foo names the parent module's Foo: {rows:?}"
    );

    let rows = run_rows(&path, "a::b::c::SupT2");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        a_widget.as_str(),
        "super::super:: walks two module levels up: {rows:?}"
    );

    let rows = run_rows(&path, "SupT3");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_resolution"], "parsed_only",
        "a top-level impl cannot have a parent module — degrade, never \
         guess a citation: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// As-of tie-breaks must be independent of store read order (PR #296 review):
// embedded reads emit temporal snapshots in commit-SHA-lexical order, not
// history order, so a last-wins-in-input-order rule can pin the OLDER
// snapshot when the newer commit's SHA sorts smaller. The tie must resolve
// from stored facts: recorded parent links identify the newest tied commit.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn as_of_tie_break_is_read_order_independent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tie_order.jsonl");
    let mut graph = Graph::new();
    // The NEWER commit (child) has the lexically SMALLER SHA, and its
    // snapshots are emitted FIRST — adversarial for both sha-max-only and
    // last-wins-in-input-order rules.
    let c_old = "9bbbcccccccccccccccccccccccccccccccccccc";
    let c_new = "0aaadddddddddddddddddddddddddddddddddddd";
    let vt = "2026-05-01T00:00:00Z";
    let p = "src/tie_order.rs";

    let new_temporal = || TemporalMetadata {
        git_commit: c_new.to_owned(),
        git_parent_commits: vec![c_old.to_owned()],
        valid_time: vt.to_owned(),
        author_time: None,
        observed_at: "2026-05-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    };

    let trait_id = sym_id(p, "OrderT");
    let impl1_id = sym_id(p, "impl OrderT for One");
    let impl2_id = sym_id(p, "impl OrderT for Two");

    // c_new snapshots FIRST in the stream.
    graph.push(
        GraphRecord::syntax_symbol(
            trait_id.clone(),
            "trait",
            p.to_owned(),
            span(1, 4),
            "OrderT".to_owned(),
            "rust",
            0,
            "Rust trait OrderT".to_owned(),
        )
        .with_temporal(new_temporal()),
    );
    for (impl_id, name, sp) in [
        (&impl1_id, "impl OrderT for One", span(6, 9)),
        (&impl2_id, "impl OrderT for Two", span(11, 14)),
    ] {
        graph.push(
            GraphRecord::syntax_symbol(
                impl_id.clone(),
                "impl",
                p.to_owned(),
                sp,
                name.to_owned(),
                "rust",
                0,
                format!("Rust impl {name}"),
            )
            .with_temporal(new_temporal()),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl_id.clone(),
                trait_id.clone(),
                None,
                format!("{name} implementation relationship"),
            )
            .with_temporal(new_temporal()),
        );
    }
    // c_old snapshots AFTER (only one implementor existed then).
    graph.push(
        GraphRecord::syntax_symbol(
            trait_id.clone(),
            "trait",
            p.to_owned(),
            span(1, 4),
            "OrderT".to_owned(),
            "rust",
            0,
            "Rust trait OrderT".to_owned(),
        )
        .with_temporal(temporal(c_old, vt)),
    );
    graph.push(
        GraphRecord::syntax_symbol(
            impl1_id.clone(),
            "impl",
            p.to_owned(),
            span(6, 9),
            "impl OrderT for One".to_owned(),
            "rust",
            0,
            "Rust impl impl OrderT for One".to_owned(),
        )
        .with_temporal(temporal(c_old, vt)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            impl1_id,
            trait_id,
            None,
            "impl OrderT for One implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c_old, vt)),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let stdout = egregore()
        .args(["query", "implementors", "OrderT", "--as-of", vt, "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "the recorded parent link identifies c_new as the tied pair's \
         newest snapshot regardless of stream order or SHA order: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["record_id"] == impl2_id.as_str()),
        "impl2 (present only at the newer commit) must appear: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Unqualified impl targets prefer the impl's own module (PR #296 review):
// Rust resolves an unqualified `Foo` inside `outer::inner` to
// `outer::inner::Foo` when it exists — a same-file top-level `Foo` decoy
// must not outrank it. When the module has no such type, the existing bare
// rules still apply (use-imported types keep resolving as before).
// ---------------------------------------------------------------------------

#[test]
fn unqualified_type_prefers_impl_module_scope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("module_scope.jsonl");
    let mut graph = Graph::new();

    let a = "src/scope_a.rs";
    let b = "src/scope_b.rs";
    let trait_id = push_symbol(&mut graph, a, "outer::inner::ScopeT", "trait", span(2, 5));
    // Adversarial decoy: a top-level Foo in the SAME FILE as the impl.
    let decoy_top_foo = push_symbol(&mut graph, a, "Foo", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        a,
        "outer::inner::impl ScopeT for Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "outer::inner::impl ScopeT for Foo implementation relationship".to_owned(),
    ));
    // The in-scope type lives in the impl's own module, in another file.
    let module_foo = push_symbol(&mut graph, b, "outer::inner::Foo", "struct", span(3, 5));

    // Fallback control: an impl in a module with NO same-named local type
    // must still resolve via the bare rules.
    let fb_trait = push_symbol(&mut graph, a, "outer::inner::FallT", "trait", span(17, 20));
    let fb_impl = push_symbol(
        &mut graph,
        a,
        "outer::inner::impl FallT for Bare",
        "impl",
        span(22, 26),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        fb_impl,
        fb_trait,
        None,
        "outer::inner::impl FallT for Bare implementation relationship".to_owned(),
    ));
    let bare_id = push_symbol(&mut graph, b, "Bare", "struct", span(7, 9));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "outer::inner::ScopeT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        module_foo.as_str(),
        "the impl module's own Foo is the innermost scope and must outrank \
         the same-file top-level decoy ({decoy_top_foo}): {rows:?}"
    );
    assert_eq!(
        rows[0]["implementing_type"], "outer::inner::Foo",
        "{rows:?}"
    );

    let rows = run_rows(&path, "outer::inner::FallT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        bare_id.as_str(),
        "no module-local type — the bare rules still resolve: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// The `unsafe` keyword is never a module prefix (PR #296 review): a nested
// unsafe impl is displayed `outer::unsafe impl T for Foo`, and a root one
// `unsafe impl T for self::Baz`. Module-prefix derivation must skip the
// keyword — `unsafe` is reserved and can never name a module — so
// module-scoped and `self::` needles resolve exactly as they do for plain
// impls.
// ---------------------------------------------------------------------------

#[test]
fn unsafe_impl_module_prefix_never_treated_as_module() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("unsafe_prefix.jsonl");
    let mut graph = Graph::new();

    let a = "src/uns_a.rs";
    let b = "src/uns_b.rs";

    // Nested unsafe impl: unqualified target must prefer the impl module's
    // own Foo over a same-file top-level decoy.
    let trait_id = push_symbol(&mut graph, a, "outer::UnsT", "trait", span(2, 5));
    let decoy_top_foo = push_symbol(&mut graph, a, "Foo", "struct", span(7, 9));
    let impl_id = push_symbol(
        &mut graph,
        a,
        "outer::unsafe impl UnsT for Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id,
        None,
        "outer::unsafe impl UnsT for Foo implementation relationship".to_owned(),
    ));
    let module_foo = push_symbol(&mut graph, b, "outer::Foo", "struct", span(3, 5));

    // Root unsafe impl with a self:: target: prefix is None, so `self::Baz`
    // must resolve the top-level Baz (never a phantom `unsafe::Baz`).
    let root_trait = push_symbol(&mut graph, a, "RootT", "trait", span(17, 20));
    let root_impl = push_symbol(
        &mut graph,
        a,
        "unsafe impl RootT for self::Baz",
        "impl",
        span(22, 26),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        root_impl,
        root_trait,
        None,
        "unsafe impl RootT for self::Baz implementation relationship".to_owned(),
    ));
    let baz_id = push_symbol(&mut graph, b, "Baz", "struct", span(7, 9));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "outer::UnsT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        module_foo.as_str(),
        "the impl module's own Foo must outrank the same-file top-level \
         decoy ({decoy_top_foo}) exactly as for a plain impl: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type"], "outer::Foo", "{rows:?}");

    let rows = run_rows(&path, "RootT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        baz_id.as_str(),
        "self:: at the root resolves the top-level Baz: {rows:?}"
    );
    assert_eq!(rows[0]["implementing_type_resolution"], "resolved");
}

// ---------------------------------------------------------------------------
// Non-commit snapshots cannot pin rows (PR #296 review): when --as-of
// resolves a record with only a node-level valid_time (current-tree /
// refresh records, no commit anchor), the implementor set cannot be pinned
// to a commit — silently answering with the unpinned current view would
// leak later-added implementors into the past. Sibling convention
// (transitive-callers) rejects temporal selectors without commit history;
// the answer is a machine-readable diagnostic, never a fabricated pin.
// ---------------------------------------------------------------------------

#[test]
fn as_of_without_commit_anchor_is_machine_readable_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("no_anchor.jsonl");
    let mut graph = Graph::new();
    let t1 = "2026-06-01T00:00:00Z";
    let t2 = "2026-06-02T00:00:00Z";
    let p = "src/no_anchor.rs";

    // Current-tree records: node-level valid_time only, no temporal block.
    let trait_id = sym_id(p, "AnchorlessT");
    graph.push(
        GraphRecord::syntax_symbol(
            trait_id.clone(),
            "trait",
            p.to_owned(),
            span(1, 4),
            "AnchorlessT".to_owned(),
            "rust",
            0,
            "Rust trait AnchorlessT".to_owned(),
        )
        .with_valid_time_inferred(t1),
    );
    let impl1_id = sym_id(p, "impl AnchorlessT for One");
    graph.push(
        GraphRecord::syntax_symbol(
            impl1_id.clone(),
            "impl",
            p.to_owned(),
            span(6, 9),
            "impl AnchorlessT for One".to_owned(),
            "rust",
            0,
            "Rust impl impl AnchorlessT for One".to_owned(),
        )
        .with_valid_time_inferred(t1),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl1_id,
        trait_id.clone(),
        None,
        "impl AnchorlessT for One implementation relationship".to_owned(),
    ));
    // A refresh later adds a second implementor at t2.
    let impl2_id = sym_id(p, "impl AnchorlessT for Two");
    graph.push(
        GraphRecord::syntax_symbol(
            impl2_id.clone(),
            "impl",
            p.to_owned(),
            span(11, 14),
            "impl AnchorlessT for Two".to_owned(),
            "rust",
            0,
            "Rust impl impl AnchorlessT for Two".to_owned(),
        )
        .with_valid_time_inferred(t2),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl2_id.clone(),
        trait_id,
        None,
        "impl AnchorlessT for Two implementation relationship".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    // --as-of at t1 resolves the trait's anchorless snapshot: the set cannot
    // be pinned, so the t2 implementor must never leak into a t1 answer —
    // and no silent unpinned answer is acceptable either.
    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "AnchorlessT",
            "--as-of",
            t1,
            "--graph",
        ])
        .arg(&path)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(
        v["error"]["code"], "no_commit_anchor",
        "an anchorless snapshot must be a machine-readable refusal: {v}"
    );
    assert!(
        !out.contains(&impl2_id),
        "the later implementor must never leak into the earlier instant: {out}"
    );
}

// ---------------------------------------------------------------------------
// As-of SHA fallback stays among sink commits (PR #296 review): in a tied
// set {parent, child, unrelated}, the parent is KNOWN not to be newest —
// the deterministic SHA fallback must choose among the remaining sinks,
// never return to the full tied set where the parent's SHA can win.
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn as_of_sha_fallback_excludes_known_parents() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("sink_fallback.jsonl");
    let mut graph = Graph::new();
    // Parent has the LARGEST SHA; its child and an unrelated head tie with it.
    let c_parent = "ffffeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let c_child = "0000ffffffffffffffffffffffffffffffffffff";
    let c_unrelated = "aaaa000000000000000000000000000000000000";
    let vt = "2026-06-03T00:00:00Z";
    let p = "src/sink_fallback.rs";

    let trait_id = sym_id(p, "SinkT");
    let impl1_id = sym_id(p, "impl SinkT for One");
    let impl2_id = sym_id(p, "impl SinkT for Two");
    let impl3_id = sym_id(p, "impl SinkT for Three");

    let temporal_with_parents = |commit: &str, parents: Vec<String>| TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents,
        valid_time: vt.to_owned(),
        author_time: None,
        observed_at: "2026-06-04T00:00:00Z".to_owned(),
        valid_time_source: None,
    };

    // Trait snapshot at each tied commit; the child records the parent link.
    for (commit, parents) in [
        (c_parent, Vec::new()),
        (c_child, vec![c_parent.to_owned()]),
        (c_unrelated, Vec::new()),
    ] {
        graph.push(
            GraphRecord::syntax_symbol(
                trait_id.clone(),
                "trait",
                p.to_owned(),
                span(1, 4),
                "SinkT".to_owned(),
                "rust",
                0,
                "Rust trait SinkT".to_owned(),
            )
            .with_temporal(temporal_with_parents(commit, parents)),
        );
    }
    // Distinct implementor sets per commit: parent {1}, child {1,2},
    // unrelated {1,3}.
    let mut add_impl = |impl_id: &String, name: &str, sp: SourceSpan, commit: &str| {
        graph.push(
            GraphRecord::syntax_symbol(
                impl_id.clone(),
                "impl",
                p.to_owned(),
                sp,
                name.to_owned(),
                "rust",
                0,
                format!("Rust impl {name}"),
            )
            .with_temporal(temporal_with_parents(commit, Vec::new())),
        );
        graph.push(
            GraphRecord::edge(
                EdgeLabel::Implements,
                impl_id.clone(),
                trait_id.clone(),
                None,
                format!("{name} implementation relationship"),
            )
            .with_temporal(temporal_with_parents(commit, Vec::new())),
        );
    };
    add_impl(&impl1_id, "impl SinkT for One", span(6, 9), c_parent);
    add_impl(&impl1_id, "impl SinkT for One", span(6, 9), c_child);
    add_impl(&impl2_id, "impl SinkT for Two", span(11, 14), c_child);
    add_impl(&impl1_id, "impl SinkT for One", span(6, 9), c_unrelated);
    add_impl(&impl3_id, "impl SinkT for Three", span(16, 19), c_unrelated);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let stdout = egregore()
        .args(["query", "implementors", "SinkT", "--as-of", vt, "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = String::from_utf8(stdout)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "the parent (largest SHA, but a KNOWN non-newest commit) must never \
         win the fallback — a sink commit's two-implementor set answers: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["record_id"] == impl3_id.as_str()),
        "max SHA among the sinks (unrelated head aaaa… > child 0000…) is the \
         documented deterministic pick: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r["record_id"] != impl2_id.as_str()),
        "deterministic: the child's set is not the fallback pick here: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Tombstone-only record IDs are stale, not unknown (PR #296 review): a
// current embedded store emits active tombstones but filters out the
// deleted node itself, so a saved trait record ID used after deletion sees
// only Tombstone{deleted_id} — the documented answer is stale_handle,
// never no_match.
// ---------------------------------------------------------------------------

#[test]
fn tombstone_only_record_id_is_stale_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tombstone_only.jsonl");
    let mut graph = Graph::new();

    // The deleted trait's node is ABSENT — only its tombstone remains,
    // exactly the shape a current embedded read produces after deletion.
    let deleted_trait_id = sym_id("src/gone.rs", "GoneT");
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &deleted_trait_id]),
        schema_version: aletheia_egregore::ir::SCHEMA_VERSION,
        deleted_id: deleted_trait_id.clone(),
        summary: "trait GoneT was deleted".to_owned(),
        producer: None,
    });
    // An unrelated live record so the store is non-empty.
    push_symbol(
        &mut graph,
        "src/other.rs",
        "Unrelated",
        "struct",
        span(2, 4),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let stdout = egregore()
        .args(["query", "implementors", &deleted_trait_id, "--graph"])
        .arg(&path)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(
        v["error"]["code"], "stale_handle",
        "a tombstone-only record ID is stale, not unknown: {v}"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_deleted_trait_record_id_is_stale_handle() {
    // End-to-end embedded shape: ingest a live trait, then ingest its
    // tombstone; the current-view read keeps the tombstone but drops the
    // node, and the saved record ID must answer stale_handle.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("live.jsonl");
    let tomb_path = temp.path().join("tomb.jsonl");
    let data_dir = temp.path().join("store");

    let mut graph = Graph::new();
    let p = "src/deleted_store.rs";
    let repo_id = stable_id(&["node", "Repository", "repo-del"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-del".to_owned()),
        "Repository repo-del".to_owned(),
    ));
    push_file(&mut graph, &repo_id, p);
    let trait_id = push_symbol(&mut graph, p, "StoreGoneT", "trait", span(2, 5));
    let impl_id = push_symbol(
        &mut graph,
        p,
        "impl StoreGoneT for Thing",
        "impl",
        span(7, 11),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        impl_id,
        trait_id.clone(),
        None,
        "impl StoreGoneT for Thing implementation relationship".to_owned(),
    ));
    fs::write(&graph_path, graph.to_jsonl().expect("serialize")).expect("write");

    let mut tomb = Graph::new();
    tomb.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &trait_id]),
        schema_version: aletheia_egregore::ir::SCHEMA_VERSION,
        deleted_id: trait_id.clone(),
        summary: "trait StoreGoneT was deleted".to_owned(),
        producer: None,
    });
    fs::write(&tomb_path, tomb.to_jsonl().expect("serialize")).expect("write");

    for source in [&graph_path, &tomb_path] {
        egregore()
            .arg("ingest")
            .arg(source)
            .args(["--adapter", "embedded", "--data-dir"])
            .arg(&data_dir)
            .assert()
            .success();
    }

    let stdout = egregore()
        .args(["query", "implementors", &trait_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(stdout).expect("utf8").trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(
        v["error"]["code"], "stale_handle",
        "the deleted trait's saved record ID must answer stale_handle on \
         the embedded current view: {v}"
    );
}

// ---------------------------------------------------------------------------
// Relative qualified impl targets never suffix-match arbitrary parents
// (PR #296 review): `impl LocalT for external::Foo` resolves as
// `<impl module prefix>::external::Foo` (innermost scope) or as the
// repo-root path `external::Foo` — an unrelated `other::external::Foo`
// sharing the suffix must never be cited; with neither exact form present
// the row is parsed_only (a crate-external path).
// ---------------------------------------------------------------------------

#[test]
fn relative_qualified_target_never_suffix_matches_parents() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("relative_qualified.jsonl");
    let mut graph = Graph::new();

    let a = "src/rel_a.rs";
    let b = "src/rel_b.rs";

    // Root impl naming external::Foo; only an unrelated other::external::Foo
    // exists locally — must be parsed_only, never the suffix decoy.
    let rel_trait = push_symbol(&mut graph, a, "RelT", "trait", span(2, 5));
    let decoy = push_symbol(&mut graph, a, "other::external::Foo", "struct", span(7, 9));
    let rel_impl = push_symbol(
        &mut graph,
        a,
        "impl RelT for external::Foo",
        "impl",
        span(11, 15),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        rel_impl,
        rel_trait,
        None,
        "impl RelT for external::Foo implementation relationship".to_owned(),
    ));

    // Nested impl with a genuinely module-scoped relative path resolves.
    let rel2_trait = push_symbol(&mut graph, a, "outer::RelT2", "trait", span(17, 20));
    let rel2_impl = push_symbol(
        &mut graph,
        a,
        "outer::impl RelT2 for sub::Bar",
        "impl",
        span(22, 26),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        rel2_impl,
        rel2_trait,
        None,
        "outer::impl RelT2 for sub::Bar implementation relationship".to_owned(),
    ));
    let scoped_bar = push_symbol(&mut graph, b, "outer::sub::Bar", "struct", span(3, 5));

    // Root impl whose relative path IS a top-level module path resolves.
    let rel3_trait = push_symbol(&mut graph, a, "RelT3", "trait", span(28, 31));
    let rel3_impl = push_symbol(
        &mut graph,
        a,
        "impl RelT3 for external::Baz",
        "impl",
        span(33, 37),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::Implements,
        rel3_impl,
        rel3_trait,
        None,
        "impl RelT3 for external::Baz implementation relationship".to_owned(),
    ));
    let root_baz = push_symbol(&mut graph, b, "external::Baz", "struct", span(7, 9));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let rows = run_rows(&path, "RelT");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_ne!(
        rows[0]["implementing_type_record_id"],
        decoy.as_str(),
        "the unrelated suffix decoy must never be cited: {rows:?}"
    );
    assert_eq!(
        rows[0]["implementing_type_resolution"], "parsed_only",
        "with neither exact form present the path is crate-external: {rows:?}"
    );

    let rows = run_rows(&path, "outer::RelT2");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        scoped_bar.as_str(),
        "the module-scoped exact form resolves: {rows:?}"
    );

    let rows = run_rows(&path, "RelT3");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["implementing_type_record_id"],
        root_baz.as_str(),
        "the repo-root exact form resolves: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Pins validate before any output (PR #296 review): with --as-of and
// multiple same-named candidates, an anchorless later candidate must not
// leave earlier candidates' rows on stdout next to the error envelope —
// no partial output ever accompanies a machine-readable failure.
// ---------------------------------------------------------------------------

#[test]
fn pin_validation_precedes_all_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("mixed_pins.jsonl");
    let mut graph = Graph::new();
    let c1 = "5e5e111111111111111111111111111111111111";
    let t1 = "2026-07-01T00:00:00Z";
    let p_a = "src/mixed_a.rs";
    let p_b = "src/mixed_b.rs";

    // Deterministic red: the COMMIT-ANCHORED candidate must sort first so
    // the unfixed loop would print its row before hitting the anchorless
    // one. Assign paths so the anchored trait has the smaller record ID.
    let id_a = sym_id(p_a, "MixedPinT");
    let id_b = sym_id(p_b, "MixedPinT");
    let (anchored_path, anchorless_path) = if id_a < id_b { (p_a, p_b) } else { (p_b, p_a) };

    let anchored_trait = sym_id(anchored_path, "MixedPinT");
    graph.push(
        GraphRecord::syntax_symbol(
            anchored_trait.clone(),
            "trait",
            anchored_path.to_owned(),
            span(1, 4),
            "MixedPinT".to_owned(),
            "rust",
            0,
            "Rust trait MixedPinT".to_owned(),
        )
        .with_temporal(temporal(c1, t1)),
    );
    let anchored_impl = sym_id(anchored_path, "impl MixedPinT for One");
    graph.push(
        GraphRecord::syntax_symbol(
            anchored_impl.clone(),
            "impl",
            anchored_path.to_owned(),
            span(6, 9),
            "impl MixedPinT for One".to_owned(),
            "rust",
            0,
            "Rust impl impl MixedPinT for One".to_owned(),
        )
        .with_temporal(temporal(c1, t1)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Implements,
            anchored_impl.clone(),
            anchored_trait,
            None,
            "impl MixedPinT for One implementation relationship".to_owned(),
        )
        .with_temporal(temporal(c1, t1)),
    );
    // The anchorless namesake carries only a node-level valid_time.
    graph.push(
        GraphRecord::syntax_symbol(
            sym_id(anchorless_path, "MixedPinT"),
            "trait",
            anchorless_path.to_owned(),
            span(1, 4),
            "MixedPinT".to_owned(),
            "rust",
            0,
            "Rust trait MixedPinT".to_owned(),
        )
        .with_valid_time_inferred(t1),
    );

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let stdout = egregore()
        .args([
            "query",
            "implementors",
            "MixedPinT",
            "--as-of",
            t1,
            "--graph",
        ])
        .arg(&path)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "no partial rows may precede the error envelope: {out}"
    );
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_commit_anchor", "{v}");
    assert!(
        !out.contains(&anchored_impl),
        "the anchored candidate's row must not leak: {out}"
    );
}
