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
    Graph, LOG_SCHEMA_VERSION,
    ir::{
        EdgeLabel, ErrorSignaturePayload, EvidenceLink, FrameResolution, GraphRecord, LogPayload,
        NodeKind, SnapshotHead, SourceSnapshotPayload, SourceSpan, StackFrame, TemporalMetadata,
    },
    log_graph, log_resolve, log_stable_id, stable_id,
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

// ---------------------------------------------------------------------------
// Issue #377: `--at` commit-view scoping for name-only (module-path) frames.
//
// The file:line frame branch routes through `location_context`, which applies
// commit-view selection. The name-only branch consulted a `symbols_by_name`
// map built from EVERY Symbol node (global-tombstone-filtered only), never
// `at_commit` — so under `--at <commit>` a module-only frame could name-match a
// symbol that did not exist at the commit view (added after it, live at HEAD),
// returned as a confidence-`1.0` `resolved`. These tests scope the map through
// the same commit/HEAD view-selection the file:line branch uses.
// ---------------------------------------------------------------------------

const AT_REPO: &str = "issue377-repo";
const C0: &str = "c0sha000000";
const C1: &str = "c1sha000000";
const C2: &str = "c2sha000000";
const T1_377: &str = "2026-01-01T00:00:00Z";
const T2_377: &str = "2026-01-02T00:00:00Z";

fn at_temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// A history-backed `Symbol` snapshot at one commit. Identity is keyed on
/// `(path, name)`, so the same `name` at a different `path` is a distinct id.
fn at_symbol(name: &str, path: &str, commit: &str, valid_time: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "symbol", path, name]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(1, 10)),
        Some(name.to_owned()),
        format!("symbol {name}@{commit}"),
    )
    .with_temporal(at_temporal(commit, valid_time));
    (id, node)
}

fn at_commit_node(sha: &str, valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        stable_id(&["node", "commit", AT_REPO, sha]),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(at_temporal(sha, valid_time))
}

/// A `Repository` node whose stamped HEAD snapshot anchors the current-state
/// view (mirrors the `location_context` HEAD filter).
fn at_repo_node(head: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Repository", AT_REPO]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(AT_REPO.to_owned()),
        format!("Repository {AT_REPO}"),
    )
    .with_source_snapshot(SourceSnapshotPayload {
        head: SnapshotHead::Commit {
            sha: head.to_owned(),
        },
        dirty: false,
        repository_id: id.clone(),
        scanned_at: FIXED_TIME.to_owned(),
    });
    (id, node)
}

fn contains(parent: &str, child: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        parent.to_owned(),
        child.to_owned(),
        None,
        "contains".to_owned(),
    )
}

/// An `ErrorSignature` carrying the given backtrace frames.
fn at_signature(seed: &str, frames: Vec<StackFrame>) -> (String, GraphRecord) {
    let id = log_stable_id(&["error_signature", AT_REPO, "template-v1", seed]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::ErrorSignature,
        None,
        None,
        Some("error signature".to_owned()),
        format!("error signature {seed}"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
        fingerprint_algorithm: "template-v1".to_owned(),
        template_excerpt: format!("template {seed}"),
        severity: "error".to_owned(),
        occurrence_count: 1,
        first_seen: "2026-01-02T12:00:00Z".to_owned(),
        last_seen: "2026-01-02T13:00:00Z".to_owned(),
        frames: Some(frames),
    }))
    .with_valid_time("2026-01-02T12:00:00Z", "log_event_timestamp");
    (id, node)
}

/// A name-only (module-path, no file/line) frame.
fn name_only_frame(module_path: &str) -> StackFrame {
    StackFrame {
        frame_index: 0,
        module_path: Some(module_path.to_owned()),
        file_path: None,
        line: None,
    }
}

/// `(target_id, frame_resolution)` for every `FRAME_RESOLVES_TO` edge emitted
/// for `sig_id`, in canonical output order.
fn frame_targets(records: &[GraphRecord], sig_id: &str) -> Vec<(String, FrameResolution)> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Edge {
                label: EdgeLabel::FrameResolvesTo,
                source,
                target,
                frame_resolution: Some(res),
                ..
            } if source == sig_id => Some((target.clone(), *res)),
            _ => None,
        })
        .collect()
}

#[test]
fn module_only_frame_under_at_resolves_against_commit_view() {
    // The same simple name `handler` names DIFFERENT symbol ids across commits:
    // `src/a.rs` at c1, `src/b.rs` at c2 (HEAD). A name-only frame `app::handler`
    // must resolve to the id that existed at the requested view, never the union
    // of both (which the pre-fix map produced → a spurious `ambiguous`).
    let (id_a, sym_a) = at_symbol("handler", "src/a.rs", C1, T1_377);
    let (id_b, sym_b) = at_symbol("handler", "src/b.rs", C2, T2_377);
    let (repo_id, repo) = at_repo_node(C2);
    let (sig_id, sig) = at_signature("handler-boom", vec![name_only_frame("app::handler")]);
    let records = vec![
        repo,
        at_commit_node(C1, T1_377),
        at_commit_node(C2, T2_377),
        contains(&repo_id, &id_a),
        contains(&repo_id, &id_b),
        sym_a,
        sym_b,
        sig,
    ];

    let at_c1 = log_resolve::resolve_frames(&records, Some(C1));
    assert_eq!(
        frame_targets(&at_c1.records, &sig_id),
        vec![(id_a, FrameResolution::Resolved)],
        "`--at c1` must resolve the name-only frame to the c1 symbol id only"
    );

    let at_c2 = log_resolve::resolve_frames(&records, Some(C2));
    assert_eq!(
        frame_targets(&at_c2.records, &sig_id),
        vec![(id_b.clone(), FrameResolution::Resolved)],
        "`--at c2` must resolve the name-only frame to the c2 symbol id only"
    );

    let head = log_resolve::resolve_frames(&records, None);
    assert_eq!(
        frame_targets(&head.records, &sig_id),
        vec![(id_b, FrameResolution::Resolved)],
        "the HEAD (c2) view must resolve to the HEAD symbol id, not the pre-HEAD one"
    );
}

#[test]
fn name_only_frame_for_symbol_absent_at_commit_is_not_stale_resolved() {
    // `foo` exists only at c2 (HEAD), never at c0. A name-only frame `app::foo`
    // under `--at c0` must NOT be `resolved` to the HEAD `foo`; the commit view
    // has no such symbol, so the ladder falls to `unresolved`.
    let (foo_id, foo) = at_symbol("foo", "src/x.rs", C2, T2_377);
    let (repo_id, repo) = at_repo_node(C2);
    let (sig_id, sig) = at_signature("foo-boom", vec![name_only_frame("app::foo")]);
    let records = vec![
        repo,
        at_commit_node(C0, T1_377),
        at_commit_node(C2, T2_377),
        contains(&repo_id, &foo_id),
        foo,
        sig,
    ];

    let at_c0 = log_resolve::resolve_frames(&records, Some(C0));
    let targets = frame_targets(&at_c0.records, &sig_id);
    assert!(
        !targets
            .iter()
            .any(|(t, res)| *res == FrameResolution::Resolved && *t == foo_id),
        "a symbol absent at the commit view must never be a stale `resolved` target: {targets:?}"
    );
    assert_eq!(
        targets.iter().map(|(_, res)| *res).collect::<Vec<_>>(),
        vec![FrameResolution::Unresolved],
        "the frame must be reported `unresolved` at c0, not resolved to the HEAD symbol"
    );

    // Positive control: at c2 (where `foo` lives) it resolves to `foo`.
    let at_c2 = log_resolve::resolve_frames(&records, Some(C2));
    assert_eq!(
        frame_targets(&at_c2.records, &sig_id),
        vec![(foo_id, FrameResolution::Resolved)],
        "`--at c2` must resolve `app::foo` to the c2 symbol id"
    );
}

#[test]
fn cli_resolve_frames_at_scopes_name_only_frame() {
    // Same scenario as the unit test, driven end-to-end through the built binary
    // with `--at c1`: the emitted FRAME_RESOLVES_TO edge must target the c1 id.
    let (id_a, sym_a) = at_symbol("handler", "src/a.rs", C1, T1_377);
    let (id_b, sym_b) = at_symbol("handler", "src/b.rs", C2, T2_377);
    let (repo_id, repo) = at_repo_node(C2);
    let (sig_id, sig) = at_signature("handler-boom", vec![name_only_frame("app::handler")]);

    let code = Graph::from_records(vec![
        repo,
        at_commit_node(C1, T1_377),
        at_commit_node(C2, T2_377),
        contains(&repo_id, &id_a),
        contains(&repo_id, &id_b),
        sym_a,
        sym_b,
    ])
    .to_jsonl()
    .expect("serialize code graph");
    let log = Graph::from_records(vec![sig])
        .to_jsonl()
        .expect("serialize log graph");

    let dir = tempfile::tempdir().expect("tempdir");
    let code_path = dir.path().join("code.graph.jsonl");
    let log_path = dir.path().join("log.graph.jsonl");
    let out_path = dir.path().join("resolved.jsonl");
    fs::write(&code_path, code).expect("write code graph");
    fs::write(&log_path, log).expect("write log graph");

    egregore()
        .arg("resolve-frames")
        .arg(&log_path)
        .arg("--graph")
        .arg(&code_path)
        .arg("--at")
        .arg(C1)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let out = fs::read_to_string(&out_path).expect("read out");
    let records = parse_records(&out);
    let edges: Vec<&Value> = frame_edges(&records);
    let targets: Vec<(&str, &str)> = edges
        .iter()
        .filter(|e| e.get("source").and_then(Value::as_str) == Some(sig_id.as_str()))
        .map(|e| {
            (
                e.get("target").and_then(Value::as_str).unwrap_or_default(),
                e.get("frame_resolution")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        targets,
        vec![(id_a.as_str(), "resolved")],
        "CLI `resolve-frames --at c1` must scope the name-only frame to the c1 id"
    );
}

/// Like [`at_symbol`] but with an explicit span, so an UNCHANGED symbol can be
/// modeled across commits sharing ONE stable id (same path+name) while differing
/// in `git_commit` and span.
fn at_symbol_span(
    name: &str,
    path: &str,
    commit: &str,
    valid_time: &str,
    sp: SourceSpan,
) -> (String, GraphRecord) {
    let id = stable_id(&["node", "symbol", path, name]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(sp),
        Some(name.to_owned()),
        format!("symbol {name}@{commit}"),
    )
    .with_temporal(at_temporal(commit, valid_time));
    (id, node)
}

/// The mirrored `EvidenceLink` on the signature node citing `target_id`.
fn frame_evidence_link<'a>(
    records: &'a [GraphRecord],
    sig_id: &str,
    target_id: &str,
) -> &'a EvidenceLink {
    records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                evidence_links: Some(links),
                ..
            } if id == sig_id => links
                .iter()
                .find(|l| l.target_record_id.as_deref() == Some(target_id)),
            _ => None,
        })
        .expect("signature must carry a FRAME_RESOLVES_TO evidence link for the target")
}

#[test]
fn name_only_frame_citation_uses_commit_view_snapshot_not_first_in_slice() {
    // Codex P2 on PR #382 (follow-up to #377): an UNCHANGED symbol keeps ONE
    // stable id across two commit snapshots (same path+name), differing only in
    // `git_commit` and span. The OLDER c1 snapshot is emitted FIRST in the slice.
    // A name-only frame `app::handler` resolves to that shared id; the mirrored
    // EvidenceLink citation (`target_git_commit` / `target_span`) MUST reflect
    // the requested view's snapshot, never the emission-order-first (older) one
    // that a plain `record_by_id` returns.
    let (id, sym_c1) = at_symbol_span("handler", "src/a.rs", C1, T1_377, span(1, 10));
    let (id2, sym_c2) = at_symbol_span("handler", "src/a.rs", C2, T2_377, span(5, 25));
    assert_eq!(
        id, id2,
        "an unchanged symbol keeps one stable id across commit snapshots"
    );
    let (repo_id, repo) = at_repo_node(C2);
    let (sig_id, sig) = at_signature("handler-boom", vec![name_only_frame("app::handler")]);
    let records = vec![
        repo,
        at_commit_node(C1, T1_377),
        at_commit_node(C2, T2_377),
        contains(&repo_id, &id),
        sym_c1, // older snapshot emitted FIRST — `record_by_id` would return this
        sym_c2,
        sig,
    ];

    // `--at c2`: citation must anchor the c2 snapshot (commit c2, span 5..25).
    let at_c2 = log_resolve::resolve_frames(&records, Some(C2));
    let link_c2 = frame_evidence_link(&at_c2.records, &sig_id, &id);
    assert_eq!(
        link_c2.target_git_commit.as_deref(),
        Some(C2),
        "the `--at c2` citation must anchor the c2 snapshot, not the emission-first c1 one"
    );
    assert_eq!(
        link_c2.target_span,
        Some(span(5, 25)),
        "the `--at c2` citation span must be the c2 snapshot span"
    );

    // HEAD view (repo head == c2): same requirement.
    let head = log_resolve::resolve_frames(&records, None);
    let link_head = frame_evidence_link(&head.records, &sig_id, &id);
    assert_eq!(
        link_head.target_git_commit.as_deref(),
        Some(C2),
        "the HEAD-view citation must anchor the HEAD (c2) snapshot"
    );
    assert_eq!(
        link_head.target_span,
        Some(span(5, 25)),
        "the HEAD-view citation span must be the c2 snapshot span"
    );

    // `--at c1`: citation must anchor the c1 snapshot (commit c1, span 1..10).
    let at_c1 = log_resolve::resolve_frames(&records, Some(C1));
    let link_c1 = frame_evidence_link(&at_c1.records, &sig_id, &id);
    assert_eq!(
        link_c1.target_git_commit.as_deref(),
        Some(C1),
        "the `--at c1` citation must anchor the c1 snapshot"
    );
    assert_eq!(
        link_c1.target_span,
        Some(span(1, 10)),
        "the `--at c1` citation span must be the c1 snapshot span"
    );
}
