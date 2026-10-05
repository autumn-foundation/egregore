//! Integration tests for test-vs-production symbol roles (issue #238):
//! extraction stamps a deterministic `role` (`test` | `production`) on every
//! `Symbol` and `File` record, and `eg query symbol` / `eg query file`
//! surface it with a `--role` scope filter.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: one of each classification signal plus a production baseline
// ---------------------------------------------------------------------------

/// Labels:
/// - `test_entry`: `#[test]` fn → `test` (signal a).
/// - `tokio_entry`: `#[tokio::test]` fn → `test` (signal a, path form).
/// - `bench_entry`: `#[bench]` fn → `test` (signal a).
/// - `tests::helper` / `tests::inner::deep`: inside an inline
///   `#[cfg(test)] mod` → `test` (signal b, ancestry composition).
/// - `test_helpers::assist`: out-of-line `#[cfg(test)] mod test_helpers;`
///   → `test` (signal b, out-of-line form via the repo-wide pass); the
///   `src/test_helpers.rs` File record is `test` too.
/// - `integration_check`: `tests/integration.rs` → `test` (signal c); the
///   `tests/integration.rs` File record is `test`.
/// - `bench_main`: `benches/bench.rs` → `test` (signal c).
/// - `production_api`: ordinary fn → `production`; `src/lib.rs` File → `production`.
const LIB_RS: &str = r"pub fn production_api() -> usize {
    1
}

#[test]
fn test_entry() {
    assert_eq!(production_api(), 1);
}

#[tokio::test]
async fn tokio_entry() {}

#[bench]
fn bench_entry() {}

#[cfg(test)]
mod tests {
    pub fn helper() -> usize {
        2
    }

    mod inner {
        pub fn deep() -> usize {
            3
        }
    }
}

#[cfg(test)]
mod test_helpers;
";

const TEST_HELPERS_RS: &str = r"pub fn assist() -> usize {
    4
}
";

const INTEGRATION_RS: &str = r"fn integration_check() {
    assert_eq!(2 + 2, 4);
}
";

const BENCH_RS: &str = r"fn bench_main() {}
";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::create_dir_all(dir.join("tests")).expect("tests dir");
    fs::create_dir_all(dir.join("benches")).expect("benches dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/test_helpers.rs"), TEST_HELPERS_RS).expect("test_helpers.rs");
    fs::write(dir.join("tests/integration.rs"), INTEGRATION_RS).expect("integration.rs");
    fs::write(dir.join("benches/bench.rs"), BENCH_RS).expect("bench.rs");
}

/// Scans the labeled fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("role-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_query(graph: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = egregore();
    cmd.arg("query");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--graph");
    cmd.arg(graph);
    cmd.assert()
}

/// The CLI emits one JSON object per line (JSONL), not a JSON array.
fn run_query_jsonl(graph: &Path, args: &[&str]) -> Vec<Value> {
    let output = run_query(graph, args).success().get_output().stdout.clone();
    let text = std::str::from_utf8(&output).expect("utf8");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each stdout line is valid JSON"))
        .collect()
}

fn run_query_text(graph: &Path, args: &[&str]) -> String {
    let output = run_query(graph, args).success().get_output().stdout.clone();
    String::from_utf8(output).expect("utf8")
}

/// Every `query symbol` row for `name` across the JSONL output.
fn symbol_rows<'a>(rows: &'a [Value], name: &str) -> Vec<&'a Value> {
    rows.iter().filter(|row| row["name"] == name).collect()
}

fn role_of(rows: &[Value], name: &str) -> String {
    let matches = symbol_rows(rows, name);
    assert_eq!(matches.len(), 1, "exactly one row for `{name}`");
    matches[0]["role"]
        .as_str()
        .expect("role field present")
        .to_owned()
}

fn graph_records(graph: &Path) -> Vec<Value> {
    let text = fs::read_to_string(graph).expect("read graph");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSONL record"))
        .collect()
}

fn file_role(records: &[Value], path: &str) -> String {
    records
        .iter()
        .find(|r| r["kind"] == "File" && r["repo_relative_path"] == path)
        .unwrap_or_else(|| panic!("File record for `{path}` should exist"))["role"]
        .as_str()
        .expect("File role present")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Extraction: every signal classifies correctly, zero false positives/negatives
// ---------------------------------------------------------------------------

#[test]
fn symbol_roles_classify_each_signal() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_query_jsonl(&graph, &["symbol", "production_api"]);
    assert_eq!(role_of(&parsed, "production_api"), "production");

    for name in ["test_entry", "tokio_entry", "bench_entry"] {
        let parsed = run_query_jsonl(&graph, &["symbol", name]);
        assert_eq!(role_of(&parsed, name), "test", "{name} is test");
    }

    let parsed = run_query_jsonl(&graph, &["symbol", "tests::helper"]);
    assert_eq!(role_of(&parsed, "tests::helper"), "test");
    let parsed = run_query_jsonl(&graph, &["symbol", "tests::inner::deep"]);
    assert_eq!(role_of(&parsed, "tests::inner::deep"), "test");
    let parsed = run_query_jsonl(&graph, &["symbol", "test_helpers::assist"]);
    assert_eq!(role_of(&parsed, "test_helpers::assist"), "test");
    let parsed = run_query_jsonl(&graph, &["symbol", "integration_check"]);
    assert_eq!(role_of(&parsed, "integration_check"), "test");
    let parsed = run_query_jsonl(&graph, &["symbol", "bench_main"]);
    assert_eq!(role_of(&parsed, "bench_main"), "test");
}

#[test]
fn file_roles_classify_each_signal() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    assert_eq!(file_role(&records, "src/lib.rs"), "production");
    assert_eq!(file_role(&records, "src/test_helpers.rs"), "test");
    assert_eq!(file_role(&records, "tests/integration.rs"), "test");
    assert_eq!(file_role(&records, "benches/bench.rs"), "test");
}

// ---------------------------------------------------------------------------
// Query surface: role on rows (JSON + text) and the --role scope filter
// ---------------------------------------------------------------------------

#[test]
fn role_scope_filter_excludes_test_symbols_from_production() {
    let (_temp, graph) = fixture_graph();
    // A test symbol under --role production is a no-match (exit 2).
    run_query(&graph, &["symbol", "test_entry", "--role", "production"])
        .failure()
        .code(2);
    // ... and vice versa.
    run_query(&graph, &["symbol", "production_api", "--role", "test"])
        .failure()
        .code(2);
    // The matching scope succeeds and the row still carries the role.
    let parsed = run_query_jsonl(&graph, &["symbol", "test_entry", "--role", "test"]);
    assert_eq!(role_of(&parsed, "test_entry"), "test");
    let parsed = run_query_jsonl(
        &graph,
        &["symbol", "production_api", "--role", "production"],
    );
    assert_eq!(role_of(&parsed, "production_api"), "production");
}

#[test]
fn role_default_all_preserves_unscoped_output() {
    let (_temp, graph) = fixture_graph();
    // Default is `all`: the test symbol is returned without the flag.
    let parsed = run_query_jsonl(&graph, &["symbol", "test_entry"]);
    assert_eq!(role_of(&parsed, "test_entry"), "test");
    let parsed = run_query_jsonl(&graph, &["symbol", "test_entry", "--role", "all"]);
    assert_eq!(role_of(&parsed, "test_entry"), "test");
}

#[test]
fn role_scope_filter_applies_to_query_file() {
    let (_temp, graph) = fixture_graph();
    // tests/integration.rs defines only a test symbol: --role production is
    // a no-match, --role test succeeds.
    run_query(
        &graph,
        &["file", "tests/integration.rs", "--role", "production"],
    )
    .failure()
    .code(2);
    let parsed = run_query_jsonl(&graph, &["file", "tests/integration.rs", "--role", "test"]);
    assert_eq!(role_of(&parsed, "integration_check"), "test");
    // src/lib.rs mixes both: --role production keeps only production_api.
    let parsed = run_query_jsonl(&graph, &["file", "src/lib.rs", "--role", "production"]);
    let names: Vec<&str> = parsed
        .iter()
        .map(|row| row["name"].as_str().expect("name"))
        .collect();
    assert!(
        names.contains(&"production_api"),
        "production symbol survives, got {names:?}"
    );
    assert!(
        !names.iter().any(|n| *n != "production_api"),
        "no test symbol survives --role production, got {names:?}"
    );
}

#[test]
fn role_surfaces_in_text_format() {
    let (_temp, graph) = fixture_graph();
    let text = run_query_text(&graph, &["symbol", "test_entry", "--format", "text"]);
    assert!(
        text.contains("role: test"),
        "text render carries the role, got:\n{text}"
    );
    let text = run_query_text(&graph, &["symbol", "production_api", "--format", "text"]);
    assert!(
        text.contains("role: production"),
        "text render carries the role, got:\n{text}"
    );
}

#[test]
fn role_values_are_byte_stable_across_scans() {
    // Five consecutive scans of the unchanged fixture yield byte-identical
    // role values (issue #238: stable over at least five scans).
    let mut scans: Vec<Vec<(String, String, String)>> = Vec::new();
    for _ in 0..5 {
        let (_temp, graph) = fixture_graph();
        let records = graph_records(&graph);
        scans.push(roles_of(&records));
    }
    for (i, scan) in scans.iter().enumerate().skip(1) {
        assert_eq!(&scans[0], scan, "scan {i} differs from scan 0");
    }
    assert!(
        scans[0].iter().all(|(_, _, role)| !role.is_empty()),
        "every Symbol/File record carries a role"
    );
}

/// `(record id, kind, role)` for every Symbol/File record, sorted.
fn roles_of(records: &[Value]) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = records
        .iter()
        .filter(|r| r["kind"] == "Symbol" || r["kind"] == "File")
        .map(|r| {
            (
                r["id"].as_str().unwrap_or("").to_owned(),
                r["kind"].as_str().unwrap_or("").to_owned(),
                r["role"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}
