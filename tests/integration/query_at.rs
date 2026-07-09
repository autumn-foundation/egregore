//! Integration tests for `eg query at <path>:<line>` (issue #151): resolve a
//! `file:line` location to its smallest enclosing code symbol, with the
//! enclosing chain reported outermost → innermost.
#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf, process::Stdio};

use aletheia_egregore::{
    GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
    scan_repository_at_with_override, scan_repository_history_with_override,
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ---------------------------------------------------------------------------
// Fixture: Tree-sitter-scanned nested Rust source
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

/// Scans the nested fixture and writes the JSONL graph. Returns
/// (`TempDir`, graph path). Caller must keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(temp.path());
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("at-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

fn run_at_ok(graph: &Path, location: &str) -> Value {
    let output = egregore()
        .args(["query", "at", location, "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn run_at_err(graph: &Path, location: &str, expected_code: i32) -> Value {
    let output = egregore()
        .args(["query", "at", location, "--graph"])
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
// AC1: the smallest enclosing Symbol is returned with citable fields
// ---------------------------------------------------------------------------

#[test]
fn query_at_resolves_free_function() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_ok(&graph, "src/lib.rs:17");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["path"], "src/lib.rs");
    assert_eq!(parsed["line"], 17);
    let symbol = &parsed["symbol"];
    assert!(
        symbol["record_id"].as_str().is_some(),
        "symbol needs record_id"
    );
    assert_eq!(symbol["kind"], "Symbol");
    assert_eq!(symbol["name"], "free_standing");
    assert_eq!(symbol["symbol_kind"], "function");
    assert_eq!(symbol["repo_relative_path"], "src/lib.rs");
    let span = &symbol["span"];
    assert_eq!(span["start_line"], 16);
    assert_eq!(span["end_line"], 18);
}

#[test]
fn query_at_resolves_innermost_method_with_enclosing_chain() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_ok(&graph, "src/lib.rs:10");

    // Innermost: the method, not the impl, module, or file.
    let symbol = &parsed["symbol"];
    assert_eq!(symbol["symbol_kind"], "method");
    assert!(
        symbol["name"]
            .as_str()
            .expect("name")
            .contains("method_one"),
        "innermost symbol must be the method, got {symbol:?}"
    );

    // Enclosing chain outermost → innermost: module, impl, method.
    let chain = parsed["enclosing_chain"].as_array().expect("chain");
    assert_eq!(
        chain.len(),
        3,
        "module + impl + method expected, got {chain:?}"
    );
    assert_eq!(chain[0]["kind"], "Module");
    assert_eq!(chain[1]["kind"], "Symbol");
    assert_eq!(chain[1]["symbol_kind"], "impl");
    assert_eq!(chain[2]["kind"], "Symbol");
    assert_eq!(chain[2]["symbol_kind"], "method");
    // The chain's innermost entry is the primary answer.
    assert_eq!(chain[2]["record_id"], symbol["record_id"]);
    // Chain spans are weakly nested (outer contains inner).
    for pair in chain.windows(2) {
        let outer = &pair[0]["span"];
        let inner = &pair[1]["span"];
        assert!(
            outer["start_line"].as_u64() <= inner["start_line"].as_u64()
                && outer["end_line"].as_u64() >= inner["end_line"].as_u64(),
            "chain must be ordered outermost → innermost, got {chain:?}"
        );
    }
    // Every chain entry is citable.
    for entry in chain {
        assert!(entry["record_id"].as_str().is_some());
        assert!(entry["span"]["start_line"].as_u64().is_some());
    }
}

#[test]
fn query_at_resolves_struct_inside_module() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_ok(&graph, "src/lib.rs:5");

    let symbol = &parsed["symbol"];
    assert_eq!(symbol["symbol_kind"], "struct");
    assert!(
        symbol["name"].as_str().expect("name").contains("Gadget"),
        "expected the struct, got {symbol:?}"
    );
    let chain = parsed["enclosing_chain"].as_array().expect("chain");
    assert_eq!(chain.len(), 2, "module + struct expected, got {chain:?}");
    assert_eq!(chain[0]["kind"], "Module");
}

// ---------------------------------------------------------------------------
// AC3: no enclosing symbol is a deterministic typed answer, never a guess
// ---------------------------------------------------------------------------

#[test]
fn query_at_file_level_use_has_no_enclosing_symbol() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_err(&graph, "src/lib.rs:1", 2);

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
    assert_eq!(parsed["error"]["path"], "src/lib.rs");
    assert_eq!(parsed["error"]["line"], 1);
    assert!(
        parsed["error"]["file_record_id"].as_str().is_some(),
        "resolved file handle must be cited, got {parsed:?}"
    );
}

#[test]
fn query_at_blank_line_has_no_enclosing_symbol() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_err(&graph, "src/lib.rs:2", 2);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
}

#[test]
fn query_at_inter_item_gap_inside_module_is_not_guessed() {
    // Line 7 is inside `mod outer` but between items: a Module is not a
    // Symbol, and the nearest sibling symbol must never be returned.
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_err(&graph, "src/lib.rs:7", 2);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
}

#[test]
fn query_at_line_past_end_of_file_has_no_enclosing_symbol() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_err(&graph, "src/lib.rs:999", 2);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
}

#[test]
fn query_at_unknown_file_exits_2_no_match() {
    let (_temp, graph) = fixture_graph();
    let parsed = run_at_err(&graph, "src/nope.rs:3", 2);
    assert_eq!(parsed["error"]["code"], "no_match");
    assert_eq!(parsed["error"]["path"], "src/nope.rs");
}

// ---------------------------------------------------------------------------
// Malformed input exits 1 with a typed diagnostic
// ---------------------------------------------------------------------------

#[test]
fn query_at_malformed_location_exits_1() {
    let (_temp, graph) = fixture_graph();
    for location in [
        "src/lib.rs",     // no line component
        "src/lib.rs:",    // empty line component
        "src/lib.rs:abc", // non-numeric line
        "src/lib.rs:0",   // lines are 1-based
        ":5",             // empty path
    ] {
        let parsed = run_at_err(&graph, location, 1);
        assert_eq!(
            parsed["error"]["code"], "malformed_location",
            "location {location:?} must be rejected as malformed, got {parsed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Byte-for-byte stable output across runs
// ---------------------------------------------------------------------------

#[test]
fn query_at_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph();

    let run = |location: &str, code: i32| {
        egregore()
            .args(["query", "at", location, "--graph"])
            .arg(&graph)
            .assert()
            .code(code)
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(run("src/lib.rs:10", 0), run("src/lib.rs:10", 0));
    assert_eq!(run("src/lib.rs:2", 2), run("src/lib.rs:2", 2));
}

// ---------------------------------------------------------------------------
// Temporal pin: --at <commit> resolves spans as they existed at that commit
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

#[test]
fn query_at_temporal_pin_resolves_span_at_that_commit() {
    let (_temp, graph) = history_fixture();

    // Line 12 is inside the symbol at C2 …
    let output = egregore()
        .args(["query", "at", "src/lib.rs:12", "--at", C2, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["symbol"]["name"], "shifty");
    assert_eq!(parsed["symbol"]["git_commit"], C2);
    assert_eq!(parsed["symbol"]["span"]["start_line"], 10);

    // … but not at C1, where the symbol lived at lines 1–5.
    let output = egregore()
        .args(["query", "at", "src/lib.rs:12", "--at", C1, "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");

    // And line 2 resolves at C1 (unique prefix accepted).
    let output = egregore()
        .args(["query", "at", "src/lib.rs:2", "--at", "c1a", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["symbol"]["name"], "shifty");
    assert_eq!(parsed["symbol"]["git_commit"], C1);
}

#[test]
fn query_at_missing_commit_exits_2() {
    let (_temp, graph) = history_fixture();
    let output = egregore()
        .args([
            "query",
            "at",
            "src/lib.rs:2",
            "--at",
            "ffffffffffff",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["error"]["code"], "missing_commit");
}

#[test]
fn query_at_ambiguous_commit_prefix_exits_1() {
    let (_temp, graph) = history_fixture();
    // "c" prefixes both C1 and C2.
    let output = egregore()
        .args(["query", "at", "src/lib.rs:2", "--at", "c", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["error"]["code"], "ambiguous_commit_prefix");
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
fn query_at_unscoped_multi_repo_collision_fails_closed() {
    let alpha = scan_named("repo-alpha");
    let beta = scan_named("repo-beta");
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("multi.jsonl");
    fs::write(&graph, format!("{alpha}{beta}")).expect("write multi-repo graph");

    // Unscoped: the same path exists in two repositories — never pick one.
    egregore()
        .args(["query", "at", "src/lib.rs:17", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("ambiguous_repository"));

    // Scoped: resolves within the selected repository.
    let output = egregore()
        .args([
            "query",
            "at",
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
fn query_at_unknown_repo_selector_exits_1() {
    let (_temp, graph) = fixture_graph();
    egregore()
        .args([
            "query",
            "at",
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
// Default view over a scan-history graph is the HEAD snapshot, not
// latest-record-per-ID: a path deleted at HEAD must be a no_match, never a
// stale symbol (PR #306 review follow-up)
// ---------------------------------------------------------------------------

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_commit_all(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "--all", "."]);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sha = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .expect("git rev-parse should execute");
    String::from_utf8_lossy(&sha.stdout).trim().to_owned()
}

/// Real Git history where commit 1 adds `src/keep.rs` and `src/gone.rs`, and
/// commit 2 (HEAD) deletes `src/gone.rs` and moves `keeper` down two lines in
/// `src/keep.rs`. Returns (`TempDir`, history graph path, c1 SHA, c2 SHA).
fn deleted_file_history_graph() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/keep.rs"), "pub fn keeper() -> u32 { 1 }\n").expect("keep.rs");
    fs::write(repo.join("src/gone.rs"), "pub fn goner() -> u32 { 1 }\n").expect("gone.rs");
    let c1 = git_commit_all(repo, "add both files", "2026-06-01T00:00:00Z");

    git(repo, ["rm", "src/gone.rs"]);
    fs::write(
        repo.join("src/keep.rs"),
        "// moved\n\npub fn keeper() -> u32 { 2 }\n",
    )
    .expect("keep.rs v2");
    let c2 = git_commit_all(repo, "delete gone.rs, move keeper", "2026-06-02T00:00:00Z");

    let jsonl = scan_repository_history_with_override(repo, Some("at-history-fixture"))
        .expect("history scan")
        .to_jsonl()
        .expect("serialize");
    let graph = repo.join("history.graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph, c1, c2)
}

#[test]
fn query_at_default_view_is_no_match_for_path_deleted_at_head() {
    let (_temp, graph, c1, _c2) = deleted_file_history_graph();

    // Default (current-state) view: the path no longer exists at HEAD, so the
    // answer is a typed no_match — never the stale pre-deletion symbol.
    let parsed = run_at_err(&graph, "src/gone.rs:1", 2);
    assert_eq!(
        parsed["error"]["code"], "no_match",
        "a HEAD-deleted path must be a no_match in the default view, got {parsed:?}"
    );

    // The temporal pin still reaches the pre-deletion state.
    let output = egregore()
        .args(["query", "at", "src/gone.rs:1", "--at", &c1, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["symbol"]["name"], "gone::goner");
    assert_eq!(parsed["symbol"]["git_commit"], c1.as_str());
}

#[test]
fn query_at_default_view_over_history_graph_resolves_head_spans() {
    let (_temp, graph, _c1, c2) = deleted_file_history_graph();

    // At HEAD, `keeper` starts on line 3; the default view must answer with
    // the HEAD version.
    let parsed = run_at_ok(&graph, "src/keep.rs:3");
    assert_eq!(parsed["symbol"]["name"], "keep::keeper");
    assert_eq!(parsed["symbol"]["git_commit"], c2.as_str());
    assert_eq!(parsed["symbol"]["span"]["start_line"], 3);

    // Line 1 is a comment at HEAD (it was inside `keeper` only at c1): the
    // default view must not resurrect the superseded span.
    let parsed = run_at_err(&graph, "src/keep.rs:1", 2);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
}

// ---------------------------------------------------------------------------
// Legacy fallback (no stamped source snapshot): newest version per ID must be
// chosen by valid time, not by record emission order — an embedded store
// emits temporal snapshots in commit-SHA lexical order (PR #306 review
// follow-up, round 2)
// ---------------------------------------------------------------------------

/// Legacy history store (no `Repository` source-snapshot stamp) where the
/// NEWER commit's SHA (`aaa…`) sorts lexically before the OLDER commit's
/// (`zzz…`), and records are emitted in commit-SHA order exactly as
/// `EmbeddedAletheiaSink::read_all_records()` does. Symbol `shifty` sits at
/// lines 1–5 in the old commit and lines 10–20 in the new one.
fn sha_ordered_legacy_fixture() -> (tempfile::TempDir, PathBuf) {
    const OLD_COMMIT: &str = "zzzz00000000"; // T1, span 1–5
    const NEW_COMMIT: &str = "aaaa00000000"; // T2, span 10–20 — sorts FIRST

    let temp = tempfile::tempdir().expect("temp dir");
    let path = "src/lib.rs";
    let file_id = stable_id(&["node", "file", "repo_legacy", path]);
    let sym_id = stable_id(&["node", "symbol", "repo_legacy", path, "shifty"]);

    // Serialize lines by hand in commit-SHA lexical order — the NEWER commit
    // first — because `Graph::to_jsonl` sorts lines and would not preserve
    // the embedded store's emission order that this regression reproduces.
    // An order-dependent keep-last dedupe would wrongly keep the OLDER
    // version here.
    let mut lines: Vec<String> = Vec::new();
    for (commit_sha, valid_time, span) in [
        (NEW_COMMIT, T2, line_span(10, 20)),
        (OLD_COMMIT, T1, line_span(1, 5)),
    ] {
        let records = [
            GraphRecord::node(
                stable_id(&["node", "commit", "repo_legacy", commit_sha]),
                NodeKind::Commit,
                None,
                None,
                Some(commit_sha.to_owned()),
                format!("Commit {commit_sha}"),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
            GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some(path.to_owned()),
                None,
                Some(path.to_owned()),
                format!("Rust source file {path}"),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
            GraphRecord::node(
                sym_id.clone(),
                NodeKind::Symbol,
                Some(path.to_owned()),
                Some(span),
                Some("shifty".to_owned()),
                "Symbol shifty".to_owned(),
            )
            .with_temporal(temporal(commit_sha, valid_time)),
        ];
        for record in records {
            lines.push(serde_json::to_string(&record).expect("record serializes"));
        }
    }

    let graph_path = temp.path().join("legacy.jsonl");
    fs::write(&graph_path, format!("{}\n", lines.join("\n"))).expect("write graph");
    (temp, graph_path)
}

#[test]
fn query_at_legacy_fallback_prefers_newest_valid_time_over_emission_order() {
    let (_temp, graph) = sha_ordered_legacy_fixture();

    // Line 12 is inside the symbol only in the NEWEST version (T2): the
    // default view must resolve it even though that record is emitted first.
    let parsed = run_at_ok(&graph, "src/lib.rs:12");
    assert_eq!(parsed["symbol"]["name"], "shifty");
    assert_eq!(
        parsed["symbol"]["span"]["start_line"], 10,
        "the newest (T2) span must win, got {parsed:?}"
    );
    assert_eq!(parsed["symbol"]["git_commit"], "aaaa00000000");

    // Line 2 was inside the symbol only in the SUPERSEDED (T1) version: the
    // default view must not resurrect it.
    let parsed = run_at_err(&graph, "src/lib.rs:2", 2);
    assert_eq!(parsed["error"]["code"], "no_enclosing_symbol");
}

// ---------------------------------------------------------------------------
// --data-dir reads must leave the live embedded store byte-for-byte untouched
// (PR #306 review follow-up, round 3): opening the embedded engine in place
// re-persists its on-disk index files, so the query must read a throwaway
// copy (same contract as the other read-only query lanes)
// ---------------------------------------------------------------------------

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`
/// (mirrors `tests/integration/evidence_freshness.rs`).
#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
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
fn query_at_data_dir_is_strictly_read_only() {
    let (temp, graph) = fixture_graph();
    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    let output = egregore()
        .args(["query", "at", "src/lib.rs:17", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim()).expect("json");
    assert_eq!(parsed["symbol"]["name"], "free_standing");

    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "query at must leave the live embedded store byte-for-byte untouched"
    );
}
