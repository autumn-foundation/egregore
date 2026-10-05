//! End-to-end tests for `eg query uses <path>` (issue #258): a read-only
//! usage-site lookup for external dependency symbols, resolved through the
//! extractor-minted `CALLS`/`Diagnostic` records and the file's `use`
//! declarations (alias-aware), with comment/string exclusion, repo scoping,
//! temporal selectors, and the external/internal boolean.
#![allow(missing_docs, clippy::doc_markdown)]

use std::{fs, path::Path};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

/// Scans a fixture crate with three usage sites of external symbols plus
/// comment/string decoys, and returns the path of the written graph JSONL.
fn seed() -> (tempfile::TempDir, std::path::PathBuf) {
    seed_with_repo_id("dep-usage-fixture")
}

fn seed_with_repo_id(repo_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let root = temp.path();
    write_fixture(
        root,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"dep_usage_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/main.rs",
                r#"use tokio::spawn;
use aletheiadb::Store as Backend;

fn main() {
    // An alias-imported call: `spawn` resolves through `use tokio::spawn`.
    spawn(async {});
    // A fully-qualified external call: no import needed.
    tokio::spawn(async {});
    // A renamed-alias path call: `Backend` resolves to `aletheiadb::Store`.
    let _db = Backend::open("/tmp/x");
    // An unresolved crate-relative call: repo-internal by construction.
    crate::missing::thing();
    // tokio::spawn is only mentioned in this comment and must not match.
    let _decoy = "aletheiadb::Store"; // a string literal, also not a usage site.
}
"#,
            ),
        ],
    );
    let graph = scan_repository_at_with_override(root, FIXED_TIME, Some(repo_id))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let path = root.join("graph.jsonl");
    fs::write(&path, graph).expect("fixture graph should be written");
    (temp, path)
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

fn run(args: &[&str]) -> assert_cmd::assert::Assert {
    egregore().args(args).assert()
}

fn run_ok(args: &[&str]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let stdout = run(args).success().get_output().stdout.clone();
    parse_ndjson(&stdout)
}

fn assert_row_shape(row: &serde_json::Value) {
    for key in [
        "record_id",
        "schema_version",
        "repo_relative_path",
        "span",
        "callee",
        "import_path",
        "external",
        "trust",
    ] {
        assert!(row.get(key).is_some(), "row is missing {key}: {row}");
    }
}

// ---------------------------------------------------------------------------
// Alias-aware usage lookup.
// ---------------------------------------------------------------------------

#[test]
fn uses_tokio_spawn_returns_alias_and_qualified_sites_but_no_decoys() {
    let (_temp, graph) = seed();
    let (header, rows) = run_ok(&[
        "query",
        "uses",
        "tokio::spawn",
        "--graph",
        graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["handle"], "tokio::spawn");
    // Exactly the two real call sites: the `use tokio::spawn` alias call and
    // the fully-qualified call. The comment and string decoys mint no
    // extractor records, so they are invisible here.
    assert_eq!(
        rows.len(),
        2,
        "expected exactly the two real tokio::spawn sites, got: {rows:?}"
    );
    let callees: Vec<&str> = rows
        .iter()
        .map(|r| r["callee"].as_str().expect("callee is a string"))
        .collect();
    assert!(
        callees.contains(&"spawn"),
        "alias call site present: {callees:?}"
    );
    assert!(
        callees.contains(&"tokio::spawn"),
        "qualified call site present: {callees:?}"
    );
    for row in &rows {
        assert_row_shape(row);
        assert_eq!(row["repo_relative_path"], "src/main.rs");
        assert_eq!(row["import_path"], "tokio::spawn");
        assert_eq!(row["external"], true);
        assert!(row["span"]["start_line"].as_u64().is_some());
    }
}

#[test]
fn uses_partial_prefix_resolves_renamed_alias_path_call() {
    let (_temp, graph) = seed();
    let (header, rows) = run_ok(&[
        "query",
        "uses",
        "aletheiadb",
        "--graph",
        graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(rows.len(), 1, "one renamed-alias site, got: {rows:?}");
    let row = &rows[0];
    assert_row_shape(row);
    // `Backend::open` written through `use aletheiadb::Store as Backend`.
    assert_eq!(row["callee"], "Backend::open");
    assert_eq!(row["import_path"], "aletheiadb::Store::open");
    assert_eq!(row["external"], true);
}

#[test]
fn uses_segment_boundary_prevents_sibling_bleed() {
    let (_temp, graph) = seed();
    // `tokio::spaw` is not a segment-aware prefix of `tokio::spawn`.
    let output = run(&[
        "query",
        "uses",
        "tokio::spaw",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    let assert = output.failure().code(2);
    let (header, rows) = parse_ndjson(&assert.get_output().stdout.clone());
    assert_eq!(header["ok"], false);
    assert_eq!(header["error"]["code"], "no_match");
    assert!(rows.is_empty());
}

// ---------------------------------------------------------------------------
// External / internal disambiguation.
// ---------------------------------------------------------------------------

#[test]
fn uses_internal_hit_is_marked_and_points_at_callers() {
    let (_temp, graph) = seed();
    let (header, rows) = run_ok(&[
        "query",
        "uses",
        "crate::missing",
        "--graph",
        graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(rows.len(), 1, "one internal site, got: {rows:?}");
    let row = &rows[0];
    assert_row_shape(row);
    assert_eq!(row["callee"], "crate::missing::thing");
    assert_eq!(row["import_path"], "crate::missing::thing");
    assert_eq!(row["external"], false);
    let hint = row["hint"].as_str().expect("internal row carries a hint");
    assert!(
        hint.contains("transitive-callers"),
        "hint points at the callers lane: {hint}"
    );
}

// ---------------------------------------------------------------------------
// Exit codes.
// ---------------------------------------------------------------------------

#[test]
fn uses_no_match_exits_2() {
    let (_temp, graph) = seed();
    let assert = run(&[
        "query",
        "uses",
        "nope::nothing",
        "--graph",
        graph.to_str().unwrap(),
    ])
    .failure()
    .code(2);
    let (header, rows) = parse_ndjson(&assert.get_output().stdout.clone());
    assert_eq!(header["ok"], false);
    assert_eq!(header["error"]["code"], "no_match");
    assert!(rows.is_empty());
}

#[test]
fn uses_malformed_path_exits_1() {
    let (_temp, graph) = seed();
    for bad in ["", "::tokio", "tokio::", "tokio::::spawn", "tok io"] {
        let assert = run(&["query", "uses", bad, "--graph", graph.to_str().unwrap()])
            .failure()
            .code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
        assert!(
            stderr.contains("malformed_path"),
            "bad path {bad:?} reports malformed_path on stderr: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Repository scoping: a multi-repo store never merges usage sites.
// ---------------------------------------------------------------------------

#[test]
fn uses_repo_scope_never_merges_across_repositories() {
    let (_temp_a, graph_a) = seed_with_repo_id("repo-a");
    let (_temp_b, graph_b) = seed_with_repo_id("repo-b");
    let merged_dir = tempfile::tempdir().expect("temp dir should be created");
    let merged = merged_dir.path().join("merged.jsonl");
    let mut contents = fs::read_to_string(&graph_a).expect("graph a");
    contents.push_str(&fs::read_to_string(&graph_b).expect("graph b"));
    fs::write(&merged, contents).expect("merged graph");

    // Unscoped sees both repositories' sites.
    let (_header, rows) = run_ok(&[
        "query",
        "uses",
        "tokio::spawn",
        "--graph",
        merged.to_str().unwrap(),
    ]);
    assert_eq!(rows.len(), 4, "two repos x two sites, got: {rows:?}");

    // Scoped sees only the chosen repository's sites.
    let (_header, rows) = run_ok(&[
        "query",
        "uses",
        "tokio::spawn",
        "--graph",
        merged.to_str().unwrap(),
        "--repo",
        "repo-a",
    ]);
    assert_eq!(rows.len(), 2, "only repo-a sites, got: {rows:?}");
}

// ---------------------------------------------------------------------------
// Deterministic ordering (issue #199).
// ---------------------------------------------------------------------------

#[test]
fn uses_rows_are_deterministically_ordered() {
    let (_temp, graph) = seed();
    let first = run_ok(&["query", "uses", "tokio", "--graph", graph.to_str().unwrap()]);
    let second = run_ok(&["query", "uses", "tokio", "--graph", graph.to_str().unwrap()]);
    assert_eq!(first.0, second.0, "headers are byte-stable");
    assert_eq!(first.1, second.1, "rows are byte-stable");
    // Sorted by (repo_relative_path, span.start_line, record_id).
    let keys: Vec<(String, u64, String)> = first
        .1
        .iter()
        .map(|r| {
            (
                r["repo_relative_path"].as_str().unwrap().to_owned(),
                r["span"]["start_line"].as_u64().unwrap(),
                r["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "rows are in canonical order");
}

// ---------------------------------------------------------------------------
// Output format.
// ---------------------------------------------------------------------------

#[test]
fn uses_text_format_is_human_readable() {
    let (_temp, graph) = seed();
    let stdout = run(&[
        "query",
        "uses",
        "tokio::spawn",
        "--graph",
        graph.to_str().unwrap(),
        "--format",
        "text",
    ])
    .success()
    .get_output()
    .stdout
    .clone();
    let text = String::from_utf8(stdout).expect("utf8");
    assert!(
        text.contains("tokio::spawn"),
        "text output names the query: {text}"
    );
    assert!(
        text.contains("src/main.rs"),
        "text output cites the file: {text}"
    );
    assert!(
        text.contains("external"),
        "text output marks the external scope: {text}"
    );
}

// ---------------------------------------------------------------------------
// Temporal selectors.
// ---------------------------------------------------------------------------

#[test]
fn uses_at_pin_on_snapshotless_graph_exits_2() {
    let (_temp, graph) = seed();
    // A plain `scan` graph has no commits: pinning one reports
    // `empty_history` with exit 2, exactly like the other temporal lanes
    // (`who-constructs` behaves identically).
    let assert = run(&[
        "query",
        "uses",
        "tokio::spawn",
        "--graph",
        graph.to_str().unwrap(),
        "--at",
        "deadbeef",
    ])
    .failure()
    .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let envelope: serde_json::Value =
        serde_json::from_str(&stdout).expect("error envelope is JSON");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "empty_history");
}
