//! Integration tests for `eg query locate <path>:<line>` (issue #212):
//! positional entry into the `eg query context` contract. `locate` resolves the
//! innermost enclosing symbol (reusing the issue #151 span-containment
//! resolver) AND returns that symbol's trust-separated cross-domain bundle —
//! the structural difference from `eg query at`, which returns only the handle.
#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::{
    EvidenceLink, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, Graph, agent_memory_stable_id, stable_id},
    scan_repository_at_with_override,
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: Tree-sitter-scanned nested Rust source (same line map as query_at)
// ---------------------------------------------------------------------------

/// Line map (1-based):
///
/// ```text
///  1  use std::fmt;                 <- file-level import, no enclosing symbol
///  2  (blank)
///  3  pub mod outer {
///  4      pub struct Gadget {
///  5          pub value: usize,
///  6      }
///  7  (blank)                       <- inside module, between items
///  8      impl Gadget {
///  9          pub fn method_one(&self) -> usize {
/// 10              let x = self.value + 1;
/// 11              x
/// 12          }
/// 13      }
/// 14  }
/// 15  (blank)
/// 16  pub fn free_standing() -> usize {
/// 17      41 + 2
/// 18  }
/// ```
const LIB_RS: &str = "use std::fmt;\n\npub mod outer {\n    pub struct Gadget {\n        pub value: usize,\n    }\n\n    impl Gadget {\n        pub fn method_one(&self) -> usize {\n            let x = self.value + 1;\n            x\n        }\n    }\n}\n\npub fn free_standing() -> usize {\n    41 + 2\n}\n";

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("src dir");
    fs::write(dir.join("src/lib.rs"), LIB_RS).expect("lib.rs");
}

fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("locate-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_locate_ok(graph: &Path, location: &str) -> Value {
    let output = egregore()
        .args(["query", "locate", location, "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn run_locate_err(graph: &Path, location: &str, expected_code: i32) -> Value {
    let output = egregore()
        .args(["query", "locate", location, "--graph"])
        .arg(graph)
        .assert()
        .code(expected_code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

// ---------------------------------------------------------------------------
// AC1/AC2: innermost symbol + the trust-separated context bundle
// ---------------------------------------------------------------------------

#[test]
fn query_locate_resolves_innermost_method_with_chain_and_bundle() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_locate_ok(&graph, "src/lib.rs:10");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["path"], "src/lib.rs");
    assert_eq!(parsed["line"], 10);

    // Innermost: the method, not the impl/module/file.
    let symbol = &parsed["symbol"];
    assert_eq!(symbol["symbol_kind"], "method");
    assert!(
        symbol["name"]
            .as_str()
            .expect("name")
            .contains("method_one"),
        "innermost symbol must be the method, got {symbol:?}"
    );
    assert!(symbol["record_id"].as_str().is_some());

    // Enclosing chain outermost → innermost: module, impl, method.
    let chain = parsed["enclosing_chain"].as_array().expect("chain");
    assert_eq!(chain.len(), 3, "module + impl + method, got {chain:?}");
    assert_eq!(chain[0]["kind"], "Module");
    assert_eq!(chain[2]["record_id"], symbol["record_id"]);

    // The differentiator from `query at`: the context bundle is present, and
    // source_facts carry the located symbol as a code fact.
    for section in [
        "source_facts",
        "observations",
        "project_state",
        "artifacts",
        "verification_evidence",
        "unresolved",
    ] {
        assert!(
            parsed[section].is_array(),
            "the context bundle must expose section `{section}`, got {parsed:?}"
        );
    }
    let source_facts = parsed["source_facts"].as_array().expect("source_facts");
    assert!(
        source_facts
            .iter()
            .any(|r| r["record_id"] == symbol["record_id"]),
        "source_facts must contain the located symbol, got {source_facts:?}"
    );
}

#[test]
fn query_locate_resolves_free_function() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_locate_ok(&graph, "src/lib.rs:17");
    assert_eq!(parsed["symbol"]["name"], "free_standing");
    assert_eq!(parsed["symbol"]["symbol_kind"], "function");
    assert_eq!(parsed["symbol"]["span"]["start_line"], 16);
}

// ---------------------------------------------------------------------------
// The context bundle actually flows agent observations (not just source facts)
// ---------------------------------------------------------------------------

/// Build a minimal current-state graph: a File, a Symbol spanning lines 5–9,
/// and an Observation evidence-linked to the symbol. This proves `locate`
/// returns the *context* bundle (`query at` never surfaces observations).
fn observation_bundle_graph() -> (tempfile::TempDir, PathBuf, String) {
    let path = "src/lib.rs";
    let repo = "bundle-repo";
    let file_id = stable_id(&["node", "file", repo, path]);
    let sym_id = "codegraph:v4:aaaa0000locatesym".to_owned();

    let file = GraphRecord::node(
        file_id,
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("Rust source file {path}"),
    );
    let symbol = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        path.to_owned(),
        SourceSpan {
            start_byte: 40,
            end_byte: 120,
            start_line: 5,
            end_line: 9,
        },
        "target_fn".to_owned(),
        format!("Rust fn target_fn at {path}:5"),
    );

    // Observation node evidence-linked to the symbol.
    let mut obs = GraphRecord::node(
        agent_memory_stable_id(&["obs", "locate1"]),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation: target_fn panics on empty input".to_owned(),
    );
    if let GraphRecord::Node {
        text: ref mut text_field,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        confidence: ref mut confidence_field,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = obs
    {
        *text_field = Some("target_fn panics on empty input".to_owned());
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence_field = Some("0.9".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = Graph::new();
    graph.push(file);
    graph.push(symbol);
    graph.push(obs);

    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("bundle.jsonl");
    fs::write(&graph_path, graph.to_jsonl().expect("serialize")).expect("write graph");
    (temp, graph_path, sym_id)
}

#[test]
fn query_locate_bundles_agent_observations_for_the_located_symbol() {
    let (_temp, graph, sym_id) = observation_bundle_graph();
    let parsed = run_locate_ok(&graph, "src/lib.rs:6");

    assert_eq!(parsed["symbol"]["record_id"], sym_id);
    let observations = parsed["observations"].as_array().expect("observations");
    assert_eq!(
        observations.len(),
        1,
        "the located symbol's observation must be bundled, got {parsed:?}"
    );
    assert!(
        observations[0]["record_id"].as_str().is_some(),
        "observation must carry a citable record_id, got {observations:?}"
    );
}

// ---------------------------------------------------------------------------
// AC4/AC5: typed absence — never a nearest-neighbor guess
// ---------------------------------------------------------------------------

#[test]
fn query_locate_file_level_use_has_no_enclosing_symbol() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_locate_err(&graph, "src/lib.rs:1", 2);
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
    assert!(
        parsed["error"]["file_record_id"].as_str().is_some(),
        "resolved file handle must be cited, got {parsed:?}"
    );
}

#[test]
fn query_locate_blank_line_and_inter_item_gap_have_no_enclosing_symbol() {
    let (_temp, graph) = fixture_graph();
    for line in ["src/lib.rs:2", "src/lib.rs:7", "src/lib.rs:15"] {
        let parsed = run_locate_err(&graph, line, 2);
        assert_eq!(
            parsed["error"]["code"], "no_enclosing_symbol",
            "line {line} must be a typed no_enclosing_symbol, got {parsed:?}"
        );
    }
}

#[test]
fn query_locate_line_past_last_span_is_out_of_range() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_locate_err(&graph, "src/lib.rs:999", 2);
    assert_eq!(parsed["error"]["code"], "line_out_of_range");
    assert_eq!(parsed["error"]["path"], "src/lib.rs");
    assert!(
        parsed["error"]["max_known_line"].as_u64().is_some(),
        "out-of-range answer must cite the recorded extent, got {parsed:?}"
    );
    assert!(parsed["error"]["file_record_id"].as_str().is_some());
}

#[test]
fn query_locate_unknown_file_exits_2_no_match() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_locate_err(&graph, "src/nope.rs:3", 2);
    assert_eq!(parsed["error"]["code"], "no_match");
    assert_eq!(parsed["error"]["path"], "src/nope.rs");
}

#[test]
fn query_locate_malformed_location_exits_1() {
    let (_temp, graph) = fixture_graph();
    for location in [
        "src/lib.rs",
        "src/lib.rs:",
        "src/lib.rs:abc",
        "src/lib.rs:0",
        ":5",
    ] {
        let parsed = run_locate_err(&graph, location, 1);
        assert_eq!(
            parsed["error"]["code"], "malformed_location",
            "location {location:?} must be rejected, got {parsed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Byte-for-byte stable output across runs
// ---------------------------------------------------------------------------

#[test]
fn query_locate_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph();
    let run = |location: &str, code: i32| {
        egregore()
            .args(["query", "locate", location, "--graph"])
            .arg(&graph)
            .assert()
            .code(code)
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(run("src/lib.rs:10", 0), run("src/lib.rs:10", 0));
    assert_eq!(run("src/lib.rs:2", 2), run("src/lib.rs:2", 2));
    assert_eq!(run("src/lib.rs:999", 2), run("src/lib.rs:999", 2));
}

// ---------------------------------------------------------------------------
// Temporal pins: --at <commit> and --as-of <instant>
// ---------------------------------------------------------------------------

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

const fn line_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start_line * 10,
        end_byte: end_line * 10 + 5,
        start_line,
        end_line,
    }
}

const C1: &str = "c1aaaa0000";
const C2: &str = "c2bbbb0000";
const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";

/// History graph where symbol `shifty` sits at lines 1–5 at commit `C1`, then
/// moves to lines 10–20 at commit `C2`.
fn history_fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = "src/lib.rs";
    let file_id = stable_id(&["node", "file", "repo_hist", path]);
    let sym_id = stable_id(&["node", "symbol", "repo_hist", path, "shifty"]);

    let mut graph = Graph::new();
    for (commit_sha, valid_time, span) in [(C1, T1, line_span(1, 5)), (C2, T2, line_span(10, 20))] {
        graph.push(
            GraphRecord::node(
                stable_id(&["node", "commit", "repo_hist", commit_sha]),
                NodeKind::Commit,
                None,
                None,
                Some(commit_sha.to_owned()),
                format!("Commit {commit_sha}"),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
        );
        graph.push(
            GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some(path.to_owned()),
                None,
                Some(path.to_owned()),
                format!("Rust source file {path}"),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
        );
        graph.push(
            GraphRecord::node(
                sym_id.clone(),
                NodeKind::Symbol,
                Some(path.to_owned()),
                Some(span),
                Some("shifty".to_owned()),
                "Symbol shifty".to_owned(),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
        );
    }

    let graph_path = temp.path().join("history.jsonl");
    fs::write(&graph_path, graph.to_jsonl().expect("serialize")).expect("write graph");
    (temp, graph_path)
}

fn run_temporal(graph: &Path, args: &[&str], code: i32) -> Value {
    let output = egregore()
        .args(["query", "locate"])
        .args(args)
        .arg("--graph")
        .arg(graph)
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json")
}

#[test]
fn query_locate_at_commit_resolves_span_at_that_commit() {
    let (_temp, graph) = history_fixture();

    // Line 12 is inside `shifty` at C2 …
    let parsed = run_temporal(&graph, &["src/lib.rs:12", "--at", C2], 0);
    assert_eq!(parsed["symbol"]["name"], "shifty");
    assert_eq!(parsed["symbol"]["git_commit"], C2);
    assert_eq!(parsed["resolved_commit"], C2);
    assert_eq!(parsed["symbol"]["span"]["start_line"], 10);
    assert!(
        parsed["source_facts"].is_array(),
        "bundle present under --at"
    );

    // … but not at C1, where the symbol lived at lines 1–5: line 12 is beyond
    // the file's last recorded structural span at that commit.
    let parsed = run_temporal(&graph, &["src/lib.rs:12", "--at", C1], 2);
    assert_eq!(parsed["error"]["code"], "line_out_of_range");
    assert_eq!(parsed["error"]["max_known_line"], 5);
}

#[test]
fn query_locate_as_of_resolves_most_recent_commit_at_or_before() {
    let (_temp, graph) = history_fixture();
    // As of just after T2, line 12 resolves to the C2 span.
    let parsed = run_temporal(
        &graph,
        &["src/lib.rs:12", "--as-of", "2026-01-03T00:00:00Z"],
        0,
    );
    assert_eq!(parsed["symbol"]["git_commit"], C2);
    assert_eq!(parsed["resolved_commit"], C2);
    // As of T1, only the C1 span (lines 1–5) exists — line 12 is past the
    // file's last recorded span at that instant.
    let parsed = run_temporal(&graph, &["src/lib.rs:12", "--as-of", T1], 2);
    assert_eq!(parsed["error"]["code"], "line_out_of_range");
    // Line 2 resolves at T1.
    let parsed = run_temporal(&graph, &["src/lib.rs:2", "--as-of", T1], 0);
    assert_eq!(parsed["symbol"]["git_commit"], C1);
}

#[test]
fn query_locate_missing_commit_exits_2() {
    let (_temp, graph) = history_fixture();
    let parsed = run_temporal(&graph, &["src/lib.rs:2", "--at", "ffffffffffff"], 2);
    assert_eq!(parsed["error"]["code"], "missing_commit");
}

#[test]
fn query_locate_ambiguous_commit_prefix_exits_1() {
    let (_temp, graph) = history_fixture();
    let parsed = run_temporal(&graph, &["src/lib.rs:2", "--at", "c"], 1);
    assert_eq!(parsed["error"]["code"], "ambiguous_commit_prefix");
}

#[test]
fn query_locate_malformed_timestamp_exits_1() {
    let (_temp, graph) = history_fixture();
    let parsed = run_temporal(&graph, &["src/lib.rs:2", "--as-of", "not-a-time"], 1);
    assert_eq!(parsed["error"]["code"], "malformed_timestamp");
}

// ---------------------------------------------------------------------------
// Repository scope
// ---------------------------------------------------------------------------

fn scan_named(repo_id: &str) -> String {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    scan_repository_at_with_override(temp.path(), FIXED_TIME, Some(repo_id))
        .expect("scan")
        .to_jsonl()
        .expect("serialize")
}

#[test]
fn query_locate_unscoped_multi_repo_collision_fails_closed() {
    let alpha = scan_named("repo-alpha");
    let beta = scan_named("repo-beta");
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{alpha}{beta}")).expect("write multi-repo graph");

    // Unscoped: the same path exists in two repositories — never pick one.
    let parsed = run_locate_err(&graph, "src/lib.rs:17", 1);
    assert_eq!(parsed["error"]["code"], "ambiguous_repository");

    // Scoped: resolves within the selected repository.
    let output = egregore()
        .args([
            "query",
            "locate",
            "src/lib.rs:17",
            "--repo",
            "repo-alpha",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["symbol"]["name"], "free_standing");
}

#[test]
fn query_locate_unknown_repo_selector_exits_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args([
            "query",
            "locate",
            "src/lib.rs:17",
            "--repo",
            "no-such-repo",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Text format
// ---------------------------------------------------------------------------

#[test]
fn query_locate_text_format_prints_symbol_and_sections() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args([
            "query",
            "locate",
            "src/lib.rs:10",
            "--format",
            "text",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("method_one").and(predicate::str::contains("source_facts=")),
        );
}
