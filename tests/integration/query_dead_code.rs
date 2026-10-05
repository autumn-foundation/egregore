//! Integration tests for `eg query dead-code` (issue #240): dead-code
//! triage candidates — symbols with zero recorded direct incoming `CALLS`
//! edges, plus one-hop dead clusters — with entry-point and public-surface
//! exclusions. Every row is a *candidate*, never proof of dead code.

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
// Fixture: the issue's hand-labeled success-metric set
// ---------------------------------------------------------------------------

/// Library root. Labels:
/// - `pub_entry`: top-level `pub fn`, no internal callers — excluded by the
///   issue #213 public-surface rule (it may be an external entry point).
/// - `it_works`: `#[test]` harness entry — excluded as a non-call entry
///   point.
/// - `ffi_hidden`: `#[no_mangle]` export — excluded as a non-call entry
///   point.
/// - `lonely`: genuinely-unreferenced private `fn` — candidate.
/// - `a` called only by `b`, `b` itself unreferenced — both candidates (the
///   one-hop dead-cluster rule, AC6).
const LIB_RS: &str = r"pub fn pub_entry() -> usize {
    1
}

fn lonely() -> usize {
    3
}

fn a() -> usize {
    4
}

fn b() -> usize {
    a() + 1
}

#[no_mangle]
fn ffi_hidden() {}

#[test]
fn it_works() {
    assert!(true);
}
";

/// Binary root: a free `fn main` is the binary's entry point — excluded,
/// even with no recorded callers.
const MAIN_RS: &str = r#"fn main() {
    println!("hello");
}
"#;

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(dir.join("src/main.rs"), MAIN_RS).expect("main.rs");
}

/// Scans the labeled fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl =
        scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("dead-code-fixture"))
            .expect("fixture should scan")
            .to_jsonl()
            .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_dead_code(graph: &Path, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "dead-code", "--graph"])
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

fn candidate_names(parsed: &Value) -> Vec<String> {
    parsed["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| c["name"].as_str().expect("name").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Success metric: 100% correct classification on the hand-labeled fixture
// ---------------------------------------------------------------------------

#[test]
fn dead_code_returns_exactly_the_labeled_candidate_set() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_dead_code(&graph, &[]);

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    assert_eq!(
        parsed["lane"], "dead-code",
        "lane id must be the stable lane name"
    );
    assert_eq!(
        parsed["classification"], "candidate",
        "every row is a candidate, never proof"
    );
    let names = candidate_names(&parsed);
    for expected in ["lonely", "a", "b"] {
        assert!(
            names
                .iter()
                .any(|n| n == expected || n.ends_with(&format!("::{expected}"))),
            "expected candidate {expected} missing from {names:?}"
        );
    }
    for excluded in ["pub_entry", "it_works", "ffi_hidden", "main"] {
        assert!(
            !names
                .iter()
                .any(|n| n == excluded || n.ends_with(&format!("::{excluded}"))),
            "excluded item {excluded} present in {names:?}"
        );
    }
    assert_eq!(names.len(), 3, "no more and no fewer than the 3 candidates");
}

#[test]
fn candidate_rows_carry_citation_fields_and_evidence() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_dead_code(&graph, &[]);

    let candidates = parsed["candidates"].as_array().expect("candidates array");
    assert!(!candidates.is_empty(), "fixture must yield candidates");
    for row in candidates {
        assert!(row["record_id"].as_str().is_some(), "row needs record_id");
        assert!(
            row["schema_version"].as_u64().is_some(),
            "row needs schema_version"
        );
        assert!(row["name"].as_str().is_some(), "row needs name");
        assert!(row["kind"].as_str().is_some(), "row needs kind");
        assert!(
            row["repo_relative_path"]
                .as_str()
                .is_some_and(|p| p.starts_with("src/")),
            "row needs a repo-relative path"
        );
        assert!(
            row["span"]["start_line"].as_u64().is_some(),
            "row needs a span"
        );
        assert_eq!(
            row["classification"], "candidate",
            "row verdict is always candidate"
        );
        assert!(
            row["evidence"]["incoming_calls"].as_u64().is_some(),
            "row carries the concrete evidence asserted"
        );
        assert_eq!(
            row["evidence"]["on_public_surface"], false,
            "candidates are asserted off the public surface"
        );
    }
}

#[test]
fn response_states_the_soundness_boundary() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_dead_code(&graph, &[]);

    let soundness = parsed["soundness"].as_str().expect("soundness metadata");
    assert!(
        soundness.contains("not proven"),
        "the lane must label results as suspected, not proven: {soundness}"
    );
    assert!(
        soundness.contains("trait objects") || soundness.contains("dynamic dispatch"),
        "the soundness boundary names what the graph cannot observe: {soundness}"
    );
}

#[test]
fn limit_truncates_rows_but_not_tallies() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_dead_code(&graph, &["--limit", "2"]);

    let candidates = parsed["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2, "--limit 2 returns two rows");
    assert_eq!(
        parsed["counts"]["candidates"], 3,
        "totals are unaffected by --limit"
    );
    assert_eq!(
        parsed["counts"]["candidates_returned"], 2,
        "returned tally tracks the truncation"
    );
}

#[test]
fn invalid_limit_is_rejected() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["query", "dead-code", "--graph"])
        .arg(&graph)
        .args(["--limit", "0"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = std::str::from_utf8(&output).expect("utf8");
    assert!(
        stderr.contains("invalid_limit"),
        "out-of-range --limit fails with a machine-readable diagnostic: {stderr}"
    );
}

#[test]
fn text_format_renders_citable_lines() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["query", "dead-code", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).expect("utf8");
    assert!(
        text.contains("candidate"),
        "text view labels rows as candidates: {text}"
    );
    assert!(
        text.contains("lonely"),
        "text view lists the candidate: {text}"
    );
    assert!(
        text.contains("src/lib.rs"),
        "text view cites the repo-relative handle: {text}"
    );
}

#[test]
fn output_is_byte_stable_across_runs() {
    let (_temp, graph) = fixture_graph();
    let first = run_dead_code(&graph, &[]);
    let second = run_dead_code(&graph, &[]);
    assert_eq!(
        serde_json::to_string(&first).expect("serialize"),
        serde_json::to_string(&second).expect("serialize"),
        "output must be byte-identical across repeated runs"
    );
}
