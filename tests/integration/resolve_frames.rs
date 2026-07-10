//! Integration tests for `eg resolve-frames` — backtrace stack-frame
//! resolution to code-graph symbols (issue #322).
//!
//! Seeds a code graph plus a log graph whose one panic backtrace exercises the
//! five adversarial frame classes and asserts each lands in its expected
//! resolution class, that ambiguity enumerates every candidate, that the node
//! evidence links agree with the emitted edges, that output is byte-identical
//! across runs, and that no raw log payload text leaks.

#![allow(missing_docs)]

use std::{fs, path::Path};

use aletheia_egregore::{
    Graph,
    ir::{GraphRecord, NodeKind, SourceSpan},
    log_graph,
};
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";
const REPO_ID: &str = "frame-fixture-repo";

// Stable code-graph record IDs (the `codegraph:` prefix keeps them out of the
// resolver's log-domain output filter).
const SYM_RESOLVED: &str = "codegraph:v1:sym_resolved";
const SYM_DUP_ONE: &str = "codegraph:v1:sym_dup_one";
const SYM_DUP_TWO: &str = "codegraph:v1:sym_dup_two";
const SYM_OPT: &str = "codegraph:v1:sym_opt";
const FILE_OPT: &str = "codegraph:v1:file_opt";

const RAW_SECRET: &str = "SUPERSECRETtokenValue1234567890";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start_line * 40,
        end_byte: end_line * 40,
        start_line,
        end_line,
    }
}

/// Builds the seeded code graph JSONL.
fn code_graph_jsonl() -> String {
    let mut g = Graph::new();
    // Case 1: a unique symbol whose span covers the resolved frame's line.
    g.push(GraphRecord::syntax_node(
        SYM_RESOLVED.to_owned(),
        NodeKind::Symbol,
        "src/alpha.rs".to_owned(),
        span(10, 20),
        "resolved_fn".to_owned(),
        "rust",
        "fn resolved_fn".to_owned(),
    ));
    // Case 2: two same-named symbols in different files → ambiguous.
    g.push(GraphRecord::syntax_node(
        SYM_DUP_ONE.to_owned(),
        NodeKind::Symbol,
        "src/one.rs".to_owned(),
        span(1, 5),
        "dup_helper".to_owned(),
        "rust",
        "fn dup_helper (one)".to_owned(),
    ));
    g.push(GraphRecord::syntax_node(
        SYM_DUP_TWO.to_owned(),
        NodeKind::Symbol,
        "src/two.rs".to_owned(),
        span(1, 5),
        "dup_helper".to_owned(),
        "rust",
        "fn dup_helper (two)".to_owned(),
    ));
    // Case 3: a file that exists but whose symbol does not cover the frame's
    // line (optimized-out frame) → path_only targeting the File node.
    g.push(GraphRecord::node(
        FILE_OPT.to_owned(),
        NodeKind::File,
        Some("src/opt.rs".to_owned()),
        None,
        None,
        "file src/opt.rs".to_owned(),
    ));
    g.push(GraphRecord::syntax_node(
        SYM_OPT.to_owned(),
        NodeKind::Symbol,
        "src/opt.rs".to_owned(),
        span(20, 30),
        "far_away".to_owned(),
        "rust",
        "fn far_away".to_owned(),
    ));
    g.to_jsonl().expect("serialize code graph")
}

/// Builds the log fixture whose single panic backtrace exercises all five
/// frame classes, plus a distinct secret-shaped error line (redaction check).
fn log_text() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    // Frame 0: file+line inside `resolved_fn` → resolved.
    // Frame 1: name-only `dup_helper` (no `at` line) → ambiguous.
    // Frame 2: file src/opt.rs line 5, no enclosing symbol → path_only.
    // Frame 3: file src/gone.rs (absent from code graph) → unresolved.
    // Frame 4: a std-library frame under /rustc/ → external (no edge).
    s.push_str("thread 'main' panicked at 'boom happened', src/alpha.rs:12:5\n");
    s.push_str("stack backtrace:\n");
    s.push_str("   0: app::alpha::resolved_fn\n");
    s.push_str("             at src/alpha.rs:12\n");
    s.push_str("   1: app::dup_helper\n");
    s.push_str("   2: app::opt::far_away\n");
    s.push_str("             at src/opt.rs:5\n");
    s.push_str("   3: app::gone::removed\n");
    s.push_str("             at src/gone.rs:7\n");
    s.push_str("   4: core::panicking::panic\n");
    s.push_str("             at /rustc/abc123/library/core/src/panicking.rs:50\n");
    // A separate secret-shaped error line (must be redacted in output).
    let _ = writeln!(
        s,
        "2026-01-02T03:11:00Z [ERROR] auth failed API_KEY={RAW_SECRET} reason denied"
    );
    s
}

/// Builds the log graph JSONL by running the real scan capture path with a
/// fixed transaction time.
fn log_graph_jsonl(repo_root: &Path) -> String {
    let log_path = repo_root.join("app.log");
    fs::write(&log_path, log_text()).expect("write log fixture");
    let scan = log_graph::scan_log_records(&log_path, repo_root, REPO_ID, FIXED_TIME, false)
        .expect("scan should succeed");
    let producer = log_graph::log_importer_producer(scan.source_format_version, FIXED_TIME);
    let mut g = Graph::new();
    for record in scan.records {
        g.push(record);
    }
    g.stamp_producer(&producer)
        .to_jsonl()
        .expect("serialize log graph")
}

fn parse_records(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect()
}

/// Runs resolve-frames and returns (out JSONL string, stdout envelope Value).
fn run_resolve(dir: &Path) -> (String, Value) {
    let code_path = dir.join("code.graph.jsonl");
    let log_path = dir.join("log.graph.jsonl");
    fs::write(&code_path, code_graph_jsonl()).expect("write code graph");
    fs::write(&log_path, log_graph_jsonl(dir)).expect("write log graph");
    let out_path = dir.join("resolved.jsonl");

    let assert = egregore()
        .arg("resolve-frames")
        .arg(&log_path)
        .arg("--graph")
        .arg(&code_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("envelope JSON");
    let out = fs::read_to_string(&out_path).expect("read out");
    (out, envelope)
}

fn frame_edges(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(Value::as_str) == Some("edge")
                && r.get("label").and_then(Value::as_str) == Some("FRAME_RESOLVES_TO")
        })
        .collect()
}

#[test]
fn resolves_five_frame_classes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, envelope) = run_resolve(dir.path());
    let records = parse_records(&out);
    let edges = frame_edges(&records);

    // Group edges by frame_index.
    let by_index = |idx: u64| -> Vec<&&Value> {
        edges
            .iter()
            .filter(|e| e.get("frame_index").and_then(Value::as_u64) == Some(idx))
            .collect()
    };

    // ── Case 1: exact unique symbol match → resolved ─────────────────────────
    let f0 = by_index(0);
    assert_eq!(f0.len(), 1, "frame 0 mints exactly one edge");
    assert_eq!(
        f0[0].get("frame_resolution").and_then(Value::as_str),
        Some("resolved")
    );
    assert_eq!(
        f0[0].get("target").and_then(Value::as_str),
        Some(SYM_RESOLVED)
    );

    // ── Case 2: two same-named symbols → ambiguous, BOTH candidates present ──
    let f1 = by_index(1);
    assert_eq!(f1.len(), 2, "frame 1 mints one edge per candidate");
    for e in &f1 {
        assert_eq!(
            e.get("frame_resolution").and_then(Value::as_str),
            Some("ambiguous")
        );
    }
    let mut targets: Vec<&str> = f1
        .iter()
        .filter_map(|e| e.get("target").and_then(Value::as_str))
        .collect();
    targets.sort_unstable();
    assert_eq!(
        targets,
        vec![SYM_DUP_ONE, SYM_DUP_TWO],
        "both ambiguous candidates are enumerated; none silently dropped"
    );

    // ── Case 3: file exists but function optimized out → path_only (File) ────
    let f2 = by_index(2);
    assert_eq!(f2.len(), 1, "frame 2 mints exactly one edge");
    assert_eq!(
        f2[0].get("frame_resolution").and_then(Value::as_str),
        Some("path_only")
    );
    assert_eq!(f2[0].get("target").and_then(Value::as_str), Some(FILE_OPT));

    // ── Case 4: path deleted since the log → unresolved (Diagnostic) ─────────
    let f3 = by_index(3);
    assert_eq!(f3.len(), 1, "frame 3 mints exactly one edge");
    assert_eq!(
        f3[0].get("frame_resolution").and_then(Value::as_str),
        Some("unresolved")
    );
    let diag_id = f3[0]
        .get("target")
        .and_then(Value::as_str)
        .expect("unresolved target");
    let diag = records
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(diag_id))
        .expect("Diagnostic node present in output");
    assert_eq!(diag.get("kind").and_then(Value::as_str), Some("Diagnostic"));
    // The Diagnostic carries the redacted frame text naming the missing path.
    let diag_text = serde_json::to_string(diag).unwrap();
    assert!(
        diag_text.contains("src/gone.rs"),
        "Diagnostic carries the frame text: {diag_text}"
    );

    // ── Case 5: dependency frame → external, tallied, ZERO edges ─────────────
    let f4 = by_index(4);
    assert!(f4.is_empty(), "an external frame mints no edge");
    let totals = &envelope["totals"];
    assert_eq!(
        totals["external"].as_u64(),
        Some(1),
        "one external frame tallied"
    );
    assert_eq!(totals["resolved"].as_u64(), Some(1));
    assert_eq!(totals["ambiguous"].as_u64(), Some(1));
    assert_eq!(totals["path_only"].as_u64(), Some(1));
    assert_eq!(totals["unresolved"].as_u64(), Some(1));
    // Per-signature tally is present and carries the external count.
    let sigs = envelope["signatures"].as_array().expect("signatures array");
    assert!(
        sigs.iter().any(|s| s["external"].as_u64() == Some(1)),
        "a per-signature external tally is reported"
    );
}

#[test]
fn node_evidence_links_agree_with_edges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, _envelope) = run_resolve(dir.path());
    let records = parse_records(&out);
    let edges = frame_edges(&records);

    // Collect (source_signature, target) pairs from edges.
    let mut edge_pairs: Vec<(String, String)> = edges
        .iter()
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_owned(),
                e["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    edge_pairs.sort();

    // Collect (signature_id, target) pairs from every ErrorSignature's
    // evidence links.
    let mut link_pairs: Vec<(String, String)> = Vec::new();
    for r in &records {
        if r.get("kind").and_then(Value::as_str) == Some("ErrorSignature")
            && let Some(links) = r.get("evidence_links").and_then(Value::as_array)
        {
            let sig_id = r["id"].as_str().unwrap().to_owned();
            for link in links {
                assert_eq!(
                    link["relation"].as_str(),
                    Some("FRAME_RESOLVES_TO"),
                    "signature evidence links are frame resolutions"
                );
                link_pairs.push((
                    sig_id.clone(),
                    link["target_record_id"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    link_pairs.sort();

    assert!(!edge_pairs.is_empty(), "some frame edges were minted");
    assert_eq!(
        edge_pairs, link_pairs,
        "node evidence links must agree with emitted edges (dual representation)"
    );
}

#[test]
fn output_is_byte_identical_across_runs() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let (out_a, env_a) = run_resolve(dir_a.path());
    let (out_b, env_b) = run_resolve(dir_b.path());
    assert_eq!(out_a, out_b, "resolved JSONL is byte-identical across runs");
    assert_eq!(
        serde_json::to_string(&env_a).unwrap(),
        serde_json::to_string(&env_b).unwrap(),
        "envelope is byte-identical across runs"
    );
}

#[test]
fn output_contains_no_raw_log_payload_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, envelope) = run_resolve(dir.path());
    assert!(
        !out.contains(RAW_SECRET),
        "the raw secret must never appear in resolved output"
    );
    assert!(
        !serde_json::to_string(&envelope)
            .unwrap()
            .contains(RAW_SECRET),
        "the raw secret must never appear in the envelope"
    );
}
