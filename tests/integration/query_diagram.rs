//! Integration tests for `eg query diagram` (issue #228).
//!
//! The fixture is a small bitemporal code graph with a labeled neighborhood:
//!
//! ```text
//!   repo:fixture
//!   ├── src/auth.rs (file:auth)
//!   │     DEFINES login(a), authenticate(b), hash_pw(c)
//!   │     IMPORTS src/crypto.rs
//!   ├── src/crypto.rs (file:crypto)
//!   ├── src/other.rs — login(d)           (same name → ambiguity)
//!   ├── src/lonely.rs — lonely(e)         (zero edges → empty neighborhood)
//!   ├── src/old.rs — old_fn(f) + tombstone (stale handle)
//!   ├── src/new.rs — newbie/newbie_caller (exist only at commit c2)
//!   └── mod auth CONTAINS login(a)
//!
//!   CALLS: authenticate(b) → login(a); login(a) → hash_pw(c);
//!          say_hi(g) → login(a); newbie_caller → newbie
//! ```
//!
//! `g` is named `say_"hi"` to prove identifier escaping in both renderers.
//! Every record carries `temporal.git_commit = c2` (except the two `Commit`
//! nodes and the tombstone), so `--at c1` selects an empty view and `--at c2`
//! the full one. Record summaries carry a sentinel string so the tests prove
//! no raw source text ever leaks into diagram output.
#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata, ir::SCHEMA_VERSION,
};
use assert_cmd::Command;
use serde_json::Value;

const BODY_SENTINEL: &str = "SENTINEL_BODY_MUST_NOT_LEAK_228";

const C1_SHA: &str = "c1111111111111111111111111111111111111111";
const C2_SHA: &str = "c2222222222222222222222222222222222222222";
const C1_TIME: &str = "2026-01-15T00:00:00Z";
const C2_TIME: &str = "2026-06-15T00:00:00Z";

/// Deterministic canonical record IDs: `codegraph:v10:` + 64 hex chars.
fn cid(c: char) -> String {
    format!(
        "codegraph:v10:{}",
        std::iter::repeat_n(c, 64).collect::<String>()
    )
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

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

fn symbol(
    id: &str,
    name: &str,
    path: &str,
    start: usize,
    end: usize,
    summary: &str,
) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(start, end)),
        Some(name.to_owned()),
        summary.to_owned(),
    )
    .with_temporal(temporal(C2_SHA, C2_TIME))
}

fn file_node(id: &str, path: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("file {path}"),
    )
    .with_temporal(temporal(C2_SHA, C2_TIME))
}

fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        None,
        format!("{} edge", label.as_str()),
    )
    .with_temporal(temporal(C2_SHA, C2_TIME))
}

/// Writes the fixture graph and returns its path (plus the owning temp dir).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("graph.jsonl");
    let jsonl = fixture_records()
        .iter()
        .map(|r| serde_json::to_string(r).expect("record serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&graph, format!("{jsonl}\n")).expect("fixture written");
    (temp, graph)
}

/// The deterministic fixture graph: a small call web around `auth` plus
/// edge-case symbols (isolated, tombstoned) and a two-commit history.
///
/// Long because it is a data literal, not logic (cf. other lane fixtures).
#[allow(clippy::too_many_lines)]
fn fixture_records() -> Vec<GraphRecord> {
    let (auth_id, login_id, verify_id, pool_id, lonely_id, old_id, helper_id) = (
        cid('a'),
        cid('b'),
        cid('c'),
        cid('d'),
        cid('e'),
        cid('f'),
        cid('g'),
    );
    let (file_auth, file_crypto, mod_auth) = (cid('1'), cid('2'), cid('3'));
    let (newbie, newbie_caller, repo) = (cid('4'), cid('5'), cid('6'));
    let records = vec![
        symbol(
            &auth_id,
            "login",
            "src/auth.rs",
            10,
            20,
            &format!("symbol login {BODY_SENTINEL}"),
        ),
        symbol(
            &login_id,
            "authenticate",
            "src/auth.rs",
            30,
            40,
            "symbol authenticate",
        ),
        symbol(
            &verify_id,
            "hash_pw",
            "src/auth.rs",
            50,
            60,
            "symbol hash_pw",
        ),
        // Same name as the anchor → ambiguity (issue #67/#192).
        symbol(
            &pool_id,
            "login",
            "src/other.rs",
            1,
            5,
            "symbol login overload",
        ),
        // Zero edges → empty neighborhood.
        symbol(
            &lonely_id,
            "lonely",
            "src/lonely.rs",
            1,
            10,
            "symbol lonely",
        ),
        // Tombstoned symbol → stale handle. Deliberately NON-temporal: under
        // the shared latest-write-wins liveness gate (issue #421) a tombstone
        // can never suppress an id carrying a bitemporal version, so the
        // tombstone is only the latest write for a current-state record.
        GraphRecord::node(
            old_id.clone(),
            NodeKind::Symbol,
            Some("src/old.rs".to_owned()),
            Some(span(1, 5)),
            Some("old_fn".to_owned()),
            "symbol old_fn".to_owned(),
        ),
        // Identifier needing renderer escaping.
        symbol(
            &helper_id,
            "say_\"hi\"",
            "src/auth.rs",
            70,
            80,
            "symbol say_hi",
        ),
        file_node(&file_auth, "src/auth.rs"),
        file_node(&file_crypto, "src/crypto.rs"),
        GraphRecord::node(
            mod_auth.clone(),
            NodeKind::Module,
            Some("src/auth.rs".to_owned()),
            Some(span(1, 80)),
            Some("auth".to_owned()),
            "module auth".to_owned(),
        )
        .with_temporal(temporal(C2_SHA, C2_TIME)),
        symbol(&newbie, "newbie", "src/new.rs", 1, 10, "symbol newbie"),
        symbol(
            &newbie_caller,
            "newbie_caller",
            "src/new.rs",
            20,
            30,
            "symbol newbie_caller",
        ),
        GraphRecord::node(
            repo.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("fixture-repo".to_owned()),
            "repository fixture-repo".to_owned(),
        )
        .with_temporal(temporal(C2_SHA, C2_TIME)),
        GraphRecord::node(
            cid('7'),
            NodeKind::Commit,
            None,
            None,
            Some(C1_SHA.to_owned()),
            "commit c1".to_owned(),
        )
        .with_temporal(temporal(C1_SHA, C1_TIME)),
        GraphRecord::node(
            cid('8'),
            NodeKind::Commit,
            None,
            None,
            Some(C2_SHA.to_owned()),
            "commit c2".to_owned(),
        )
        .with_temporal(temporal(C2_SHA, C2_TIME)),
        edge(EdgeLabel::Defines, &file_auth, &auth_id),
        edge(EdgeLabel::Defines, &file_auth, &login_id),
        edge(EdgeLabel::Defines, &file_auth, &verify_id),
        edge(EdgeLabel::Contains, &mod_auth, &auth_id),
        edge(EdgeLabel::Calls, &login_id, &auth_id),
        edge(EdgeLabel::Calls, &auth_id, &verify_id),
        edge(EdgeLabel::Calls, &helper_id, &auth_id),
        edge(EdgeLabel::Imports, &file_auth, &file_crypto),
        edge(EdgeLabel::Calls, &newbie_caller, &newbie),
        edge(EdgeLabel::Contains, &repo, &file_auth),
        edge(EdgeLabel::Contains, &repo, &file_crypto),
        // Tombstoned symbol → stale handle.
        GraphRecord::Tombstone {
            id: cid('9'),
            schema_version: SCHEMA_VERSION,
            deleted_id: old_id,
            summary: "removed".to_owned(),
            producer: None,
        },
    ];
    records
}

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn run_diagram(graph: &PathBuf, args: &[&str]) -> assert_cmd::assert::Assert {
    egregore()
        .arg("query")
        .arg("diagram")
        .args(args)
        .arg("--graph")
        .arg(graph)
        .assert()
}

fn diagram_json(graph: &PathBuf, args: &[&str]) -> Value {
    let mut all: Vec<&str> = Vec::with_capacity(args.len() + 2);
    all.extend_from_slice(args);
    all.push("--format");
    all.push("json");
    let assert = run_diagram(graph, &all).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&stdout).expect("stdout is one JSON document")
}

fn node_ids(value: &Value) -> BTreeSet<String> {
    value["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| n["record_id"].as_str().expect("record_id").to_owned())
        .collect()
}

fn edge_triples(value: &Value) -> BTreeSet<(String, String, String)> {
    value["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .map(|e| {
            (
                e["source_record_id"].as_str().expect("source").to_owned(),
                e["target_record_id"].as_str().expect("target").to_owned(),
                e["label"].as_str().expect("label").to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Precision / recall against the labeled neighborhood (success metric).
// ---------------------------------------------------------------------------

#[test]
fn depth_one_neighborhood_has_exact_expected_nodes_and_edges() {
    let (_temp, graph) = fixture();
    let value = diagram_json(&graph, &[&cid('a')]);

    assert_eq!(value["ok"], true);
    assert_eq!(value["lane"], "diagram");
    assert_eq!(value["depth"], 1);

    // Labeled expected neighborhood: the anchor, its caller(s), its callee,
    // and its DEFINES/CONTAINS containers. The `say_"hi"` caller exercises
    // renderer escaping end to end.
    let expected_nodes: BTreeSet<String> =
        [cid('a'), cid('b'), cid('c'), cid('g'), cid('1'), cid('3')]
            .into_iter()
            .collect();
    assert_eq!(node_ids(&value), expected_nodes, "node recall/precision");

    // Induced subgraph over the admitted nodes, restricted to the walk's edge
    // vocabulary (CALLS/DEFINES/CONTAINS/IMPORTS): every stored edge between
    // rendered nodes appears, and no edge appears that is not in the graph.
    let expected_edges: BTreeSet<(String, String, String)> = [
        (cid('b'), cid('a'), "CALLS".to_owned()),
        (cid('g'), cid('a'), "CALLS".to_owned()),
        (cid('a'), cid('c'), "CALLS".to_owned()),
        (cid('1'), cid('a'), "DEFINES".to_owned()),
        (cid('1'), cid('b'), "DEFINES".to_owned()),
        (cid('1'), cid('c'), "DEFINES".to_owned()),
        (cid('3'), cid('a'), "CONTAINS".to_owned()),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        edge_triples(&value),
        expected_edges,
        "edge recall/precision"
    );

    // 100% of nodes carry a citable handle: stable record ID plus a
    // repo-relative file/span.
    for node in value["nodes"].as_array().expect("nodes") {
        assert!(
            node["record_id"].as_str().is_some_and(|s| !s.is_empty()),
            "every node has a stable record ID"
        );
        assert!(
            node["repo_relative_path"].as_str().is_some(),
            "every node cites a repo-relative path"
        );
        assert!(node["key"].as_str().is_some(), "every node has a node key");
    }

    // The envelope embeds both rendered diagram texts.
    assert!(
        value["mermaid"].as_str().is_some_and(|s| !s.is_empty()),
        "JSON envelope embeds the Mermaid text"
    );
    assert!(
        value["dot"].as_str().is_some_and(|s| !s.is_empty()),
        "JSON envelope embeds the DOT text"
    );

    // Anchor is marked as the focus node in both renderers.
    let mermaid = value["mermaid"].as_str().unwrap();
    assert!(mermaid.contains("anchor"), "Mermaid marks the anchor node");
    let dot = value["dot"].as_str().unwrap();
    assert!(dot.contains("anchor"), "DOT marks the anchor node");
}

#[test]
fn depth_two_adds_the_file_import_but_nothing_else() {
    let (_temp, graph) = fixture();
    let value = diagram_json(&graph, &[&cid('a'), "--depth", "2"]);

    let mut expected = [cid('a'), cid('b'), cid('c'), cid('g'), cid('1'), cid('3')]
        .into_iter()
        .collect::<BTreeSet<_>>();
    expected.insert(cid('2')); // file:crypto via file:auth IMPORTS
    assert_eq!(node_ids(&value), expected);

    let mut edges = [
        (cid('b'), cid('a'), "CALLS".to_owned()),
        (cid('g'), cid('a'), "CALLS".to_owned()),
        (cid('a'), cid('c'), "CALLS".to_owned()),
        (cid('1'), cid('a'), "DEFINES".to_owned()),
        (cid('1'), cid('b'), "DEFINES".to_owned()),
        (cid('1'), cid('c'), "DEFINES".to_owned()),
        (cid('3'), cid('a'), "CONTAINS".to_owned()),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    edges.insert((cid('1'), cid('2'), "IMPORTS".to_owned()));
    assert_eq!(edge_triples(&value), edges);
}

// ---------------------------------------------------------------------------
// Determinism: byte-identical output across 5 consecutive runs.
// ---------------------------------------------------------------------------

#[test]
fn mermaid_output_is_byte_identical_across_five_runs() {
    let (_temp, graph) = fixture();
    let first = run_diagram(&graph, &[&cid('a'), "--format", "mermaid"])
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!first.is_empty(), "diagram output must be non-empty");
    for _ in 0..4 {
        let next = run_diagram(&graph, &[&cid('a'), "--format", "mermaid"])
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(next, first, "run output must be byte-identical");
    }
}

#[test]
fn dot_output_is_byte_identical_across_five_runs() {
    let (_temp, graph) = fixture();
    let first = run_diagram(&graph, &[&cid('a'), "--format", "dot"])
        .success()
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let next = run_diagram(&graph, &[&cid('a'), "--format", "dot"])
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(next, first, "run output must be byte-identical");
    }
}

// ---------------------------------------------------------------------------
// Diagnostics and exit codes.
// ---------------------------------------------------------------------------

#[test]
fn unknown_symbol_is_no_match_exit_2() {
    let (_temp, graph) = fixture();
    let assert = run_diagram(&graph, &["nosuchfn"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "no_match");
}

#[test]
fn ambiguous_symbol_is_exit_1_with_sorted_candidates() {
    let (_temp, graph) = fixture();
    // "login" names two live symbols (a and d) → ambiguous.
    let assert = run_diagram(&graph, &["login"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(1));
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("Ambiguous"), "stderr names the ambiguity");
    assert!(stderr.contains(&cid('a')), "candidates list the anchor");
    assert!(stderr.contains(&cid('d')), "candidates list the overload");
}

#[test]
fn empty_neighborhood_is_exit_3_not_a_lone_node_diagram() {
    let (_temp, graph) = fixture();
    let assert = run_diagram(&graph, &["lonely"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(3));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "empty_neighborhood");
}

#[test]
fn tombstoned_symbol_is_stale_handle_exit_2() {
    let (_temp, graph) = fixture();
    let assert = run_diagram(&graph, &["old_fn"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(value["error"]["code"], "stale_handle");
}

#[test]
fn zero_depth_and_zero_max_nodes_are_exit_1() {
    let (_temp, graph) = fixture();
    let assert = run_diagram(&graph, &[&cid('a'), "--depth", "0"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(1));
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("invalid_depth"), "names the bad flag");

    let assert = run_diagram(&graph, &[&cid('a'), "--max-nodes", "0"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(1));
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("invalid_max_nodes"), "names the bad flag");
}

#[test]
fn empty_handle_is_exit_1() {
    let (_temp, graph) = fixture();
    let assert = run_diagram(&graph, &[""]).failure();
    assert_eq!(assert.get_output().status.code(), Some(1));
}

// ---------------------------------------------------------------------------
// Temporal scoping (--at / --as-of reuse the bitemporal selector).
// ---------------------------------------------------------------------------

#[test]
fn at_c2_renders_but_at_c1_reports_no_match() {
    let (_temp, graph) = fixture();
    // At c2 every fixture record is live: the full depth-1 neighborhood.
    let value = diagram_json(&graph, &[&cid('a'), "--at", C2_SHA]);
    assert_eq!(value["ok"], true);
    assert_eq!(value["at_commit"], C2_SHA);
    assert_eq!(node_ids(&value).len(), 6);

    // At c1 the anchor's records do not exist → stable diagnostic, exit 2.
    let assert = run_diagram(&graph, &[&cid('a'), "--at", C1_SHA]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(
        stdout.contains("no_match"),
        "absent-at-revision is no_match"
    );
}

#[test]
fn as_of_selects_the_commit_at_or_before_the_instant() {
    let (_temp, graph) = fixture();
    // After c1 but before c2 → the c1 view, where the anchor is absent.
    let assert = run_diagram(&graph, &[&cid('a'), "--as-of", "2026-03-01T00:00:00Z"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));

    // After c2 → the c2 view renders.
    let value = diagram_json(&graph, &[&cid('a'), "--as-of", "2026-12-01T00:00:00Z"]);
    assert_eq!(value["ok"], true);
    assert_eq!(value["at_commit"], C2_SHA);
}

#[test]
fn unresolvable_temporal_selectors_have_stable_diagnostics() {
    let (_temp, graph) = fixture();
    // Unknown commit prefix → exit 2.
    let assert = run_diagram(&graph, &[&cid('a'), "--at", "deadbeef"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));

    // Malformed instant → exit 1.
    let assert = run_diagram(&graph, &[&cid('a'), "--as-of", "not-a-time"]).failure();
    assert_eq!(assert.get_output().status.code(), Some(1));

    // History-less graph + --at → exit 2 empty_history.
    let temp = tempfile::tempdir().expect("temp dir");
    let bare = temp.path().join("bare.jsonl");
    let rec = symbol(&cid('a'), "login", "src/auth.rs", 10, 20, "symbol login");
    fs::write(&bare, format!("{}\n", serde_json::to_string(&rec).unwrap())).expect("write");
    let assert = run_diagram(&bare, &[&cid('a'), "--at", C2_SHA]).failure();
    assert_eq!(assert.get_output().status.code(), Some(2));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(
        stdout.contains("empty_history"),
        "history-less store is explicit"
    );
}

// ---------------------------------------------------------------------------
// Truncation is reported explicitly, never silent.
// ---------------------------------------------------------------------------

#[test]
fn node_budget_truncation_is_reported_explicitly() {
    let (_temp, graph) = fixture();
    // Depth-1 discovery order (hop, record ID): 1, 3, b, c, g. Budget 3 keeps
    // the anchor plus file:auth and mod:auth; b, c, g are dropped with their
    // 5 incident edges (b→a, g→a, a→c, 1→b, 1→c).
    let value = diagram_json(&graph, &[&cid('a'), "--max-nodes", "3"]);
    assert_eq!(value["ok"], true);
    let expected: BTreeSet<String> = [cid('a'), cid('1'), cid('3')].into_iter().collect();
    assert_eq!(node_ids(&value), expected);

    let trunc = &value["truncation"]["node_budget_truncated"];
    assert_eq!(trunc["max_nodes"], 3);
    assert_eq!(trunc["dropped_nodes"], 3);
    assert_eq!(trunc["dropped_edges"], 5);

    // The Mermaid text carries the truncation notice, not a silent cut.
    let mermaid = value["mermaid"].as_str().expect("mermaid");
    assert!(
        mermaid.contains("truncat"),
        "truncation is visible in the diagram"
    );
}

#[test]
fn depth_bound_truncation_is_reported_explicitly() {
    let (_temp, graph) = fixture();
    // file:crypto sits at depth 2 (file:auth IMPORTS), so depth 1 truncates.
    let value = diagram_json(&graph, &[&cid('a')]);
    let trunc = &value["truncation"]["max_depth_truncated"];
    assert_eq!(trunc["max_depth"], 1);
    assert_eq!(trunc["dropped_total"], 1);
    assert_eq!(trunc["dropped_frontier"][0]["depth"], 2);
    assert_eq!(trunc["dropped_frontier"][0]["count"], 1);

    // Depth 2 admits it: no depth truncation remains.
    let value = diagram_json(&graph, &[&cid('a'), "--depth", "2"]);
    assert!(
        value["truncation"]["max_depth_truncated"].is_null(),
        "depth 2 reaches every reachable node"
    );
}

// ---------------------------------------------------------------------------
// Redaction: output never includes raw source text.
// ---------------------------------------------------------------------------

#[test]
fn no_raw_source_text_leaks_into_any_format() {
    let (_temp, graph) = fixture();
    for format in ["mermaid", "dot", "json", "text"] {
        let out = run_diagram(&graph, &[&cid('a'), "--format", format])
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(BODY_SENTINEL),
            "format {format} must not leak record summaries"
        );
    }
}

// ---------------------------------------------------------------------------
// Renderer escaping + structural validity.
// ---------------------------------------------------------------------------

#[test]
fn mermaid_escapes_special_identifiers() {
    let (_temp, graph) = fixture();
    let out = run_diagram(&graph, &[&cid('a'), "--format", "mermaid"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    assert!(
        !text.contains("say_\"hi\""),
        "raw double quote must not appear in a Mermaid label"
    );
    assert!(
        text.contains("say_#quot;hi#quot;"),
        "double quote is Mermaid-escaped"
    );
    // Structural: every edge line is `nK -- LABEL --> nJ`.
    for line in text.lines() {
        let line = line.trim();
        if line.contains("-->") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts.len(), 5, "edge line has 5 tokens: {line}");
            assert!(parts[0].starts_with('n'), "edge source is a node key");
            assert!(parts[1] == "--", "edge separator");
            assert!(
                ["CALLS", "DEFINES", "CONTAINS", "IMPORTS"].contains(&parts[2]),
                "edge label is vocabulary"
            );
            assert_eq!(parts[3], "-->", "edge arrow");
            assert!(parts[4].starts_with('n'), "edge target is a node key");
        }
    }
}

#[test]
fn dot_escapes_special_identifiers_and_is_structurally_valid() {
    let (_temp, graph) = fixture();
    let out = run_diagram(&graph, &[&cid('a'), "--format", "dot"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("digraph"), "DOT opens a digraph");
    assert!(
        text.contains("say_\\\"hi\\\""),
        "double quote is DOT-escaped"
    );
    assert_eq!(
        text.matches('{').count(),
        text.matches('}').count(),
        "braces balance"
    );
    for line in text.lines() {
        let line = line.trim();
        if line.contains("->") && line.contains("[label=") {
            assert!(line.ends_with("];"), "edge statement terminates: {line}");
        }
    }
}

#[test]
fn external_renderers_parse_the_output_when_available() {
    /// True when `name` resolves on PATH (spawn probe; no side effects).
    fn binary_available(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok()
    }

    let (_temp, graph) = fixture();
    let mermaid = run_diagram(&graph, &[&cid('a'), "--format", "mermaid"])
        .success()
        .get_output()
        .stdout
        .clone();
    let dot = run_diagram(&graph, &[&cid('a'), "--format", "dot"])
        .success()
        .get_output()
        .stdout
        .clone();

    let mut checked = 0;
    if binary_available("mmdc") {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("diagram.mmd");
        fs::write(&path, &mermaid).expect("write mmd");
        let status = std::process::Command::new("mmdc")
            .args(["-i"])
            .arg(&path)
            .args(["-o", "out.svg"])
            .current_dir(dir.path())
            .status()
            .expect("mmdc runs");
        assert!(status.success(), "mmdc must parse with zero syntax errors");
        checked += 1;
    }
    if binary_available("dot") {
        use std::io::Write as _;
        let mut child = std::process::Command::new("dot")
            .arg("-Tcanon")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("dot runs");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(&dot)
            .expect("write dot");
        let output = child.wait_with_output().expect("dot output");
        assert!(
            output.status.success(),
            "dot must parse with zero syntax errors"
        );
        checked += 1;
    }
    if checked == 0 {
        eprintln!(
            "note: neither mmdc nor dot is installed; structural assertions above \
             are the parse-clean evidence (documented gap, issue #228)"
        );
    }
}

// ---------------------------------------------------------------------------
// Read-only proof (embedded feature only).
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_fixture(graph: &PathBuf) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    (temp, data_dir)
}

#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_query_is_strictly_read_only() {
    let (_fixture_temp, graph) = fixture();
    let (_temp, data_dir) = ingest_fixture(&graph);

    let before = dir_fingerprint(&data_dir);
    for format in ["mermaid", "dot", "json", "text"] {
        egregore()
            .args([
                "query",
                "diagram",
                &cid('a'),
                "--format",
                format,
                "--data-dir",
            ])
            .arg(&data_dir)
            .assert()
            .success();
    }
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "diagram must not create or mutate any store file when reading --data-dir"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_transports_agree() {
    let (_fixture_temp, graph) = fixture();
    let (_temp, data_dir) = ingest_fixture(&graph);

    let from_graph = run_diagram(&graph, &[&cid('a'), "--format", "mermaid"])
        .success()
        .get_output()
        .stdout
        .clone();
    let from_data_dir = egregore()
        .args([
            "query",
            "diagram",
            &cid('a'),
            "--format",
            "mermaid",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        from_graph, from_data_dir,
        "--graph and --data-dir must return byte-identical diagrams"
    );
}

// ---------------------------------------------------------------------------
// Timing budget: <2s on a representative crate-sized graph.
// ---------------------------------------------------------------------------

#[test]
fn renders_under_two_seconds_on_a_crate_sized_graph() {
    // 2000 symbols in one file, chained s_i CALLS s_{i+1}; the diagram for a
    // mid-chain symbol renders its two neighbors plus the file.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("big.jsonl");
    let file = cid('0');
    let mut records = vec![file_node(&file, "src/lib.rs")];
    let sym_id = |i: usize| format!("codegraph:v10:{i:064x}");
    for i in 0..2000usize {
        records.push(
            GraphRecord::node(
                sym_id(i),
                NodeKind::Symbol,
                Some("src/lib.rs".to_owned()),
                Some(span(i + 1, i + 2)),
                Some(format!("s_{i}")),
                format!("symbol s_{i}"),
            )
            .with_temporal(temporal(C2_SHA, C2_TIME)),
        );
        records.push(edge(EdgeLabel::Defines, &file, &sym_id(i)));
        if i > 0 {
            records.push(edge(EdgeLabel::Calls, &sym_id(i - 1), &sym_id(i)));
        }
    }
    let jsonl = records
        .iter()
        .map(|r| serde_json::to_string(r).expect("record serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&graph, format!("{jsonl}\n")).expect("fixture written");

    let start = Instant::now();
    let assert = run_diagram(&graph, &[&sym_id(1000), "--format", "mermaid"]).success();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "diagram rendered in {elapsed:?}, budget is 2s"
    );
    let value = diagram_json(&graph, &[&sym_id(1000)]);
    assert_eq!(node_ids(&value).len(), 4, "s_999, s_1000, s_1001, file");
    let _ = assert;
}
