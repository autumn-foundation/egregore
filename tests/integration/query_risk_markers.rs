//! Integration tests for `eg query risk-markers` (issue #210).
//!
//! The lane surfaces the intentional completion-risk macro markers the Rust
//! extractor already records as `Diagnostic` nodes — `todo!`,
//! `unimplemented!`, `unreachable!` (category `stub`) and `panic!` (category
//! `panic`) — as a citable pre-edit triage lane. Benign macros (`println!`,
//! `vec!`, `format!`, …) are never returned (zero false-risk). Each row
//! carries its category, macro name, `Diagnostic` record ID, repo-relative
//! file/span handle, and the enclosing symbol's handle (explicit `null` at
//! module top level).
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_at_with_override, scan_repository_history_with_override};
use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(output.status.success(), "git command failed");
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let output = Command::new("git")
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
    git_output(repo, ["rev-parse", "HEAD"])
}

/// Risk macros nested inside enclosing functions, benign macros in the same
/// file, and one file of only benign macros plus one clean file.
const LIB_RS: &str = r#"pub fn finish() -> u32 {
    todo!("wire the real computation")
}

pub fn abort_early(flag: bool) -> u32 {
    if flag {
        panic!("flag must never be set");
    }
    7
}

pub fn decide(x: u32) -> u32 {
    match x {
        0 => unimplemented!("zero case"),
        _ => unreachable!("x is always positive here"),
    }
}

pub fn noisy() -> String {
    println!("just logging");
    let items = vec![1, 2, 3];
    format!("count={}", items.len())
}
"#;

/// Intentionally not valid `rustc` input: a bare macro invocation at module
/// top level. Tree-sitter is error-tolerant, so the extractor still records
/// the `todo!` diagnostic — exercising the explicit-`none` enclosing-symbol
/// case (no `DEFINES` owner exists at top level).
const TOPLEVEL_RS: &str = "todo!(\"revisit the module\");\n";

const BENIGN_RS: &str = r#"pub fn helper() -> u32 {
    let items = vec![1, 2, 3];
    println!("benign only: {}", items.len());
    42
}
"#;

const CLEAN_RS: &str = "pub fn clean() -> u32 {\n    7\n}\n";

/// Builds the seeded fixture checkout, scans it deterministically, and writes
/// the JSONL graph. Returns (`TempDir`, `graph_path`).
fn fixture_scanned() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", LIB_RS);
    write(&repo, "src/toplevel.rs", TOPLEVEL_RS);
    write(&repo, "src/benign.rs", BENIGN_RS);
    write(&repo, "src/empty/clean.rs", CLEAN_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("risk-markers-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, graph_path)
}

fn run_lane(graph: &Path, extra_args: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "risk-markers", "--graph"])
        .arg(graph)
        .args(extra_args)
        .output()
        .expect("eg should run");
    assert!(
        output.status.success(),
        "risk-markers failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be JSON")
}

fn run_lane_raw(graph: &Path, extra_args: &[&str]) -> std::process::Output {
    egregore()
        .args(["query", "risk-markers", "--graph"])
        .arg(graph)
        .args(extra_args)
        .output()
        .expect("eg should run")
}

/// (category, `macro_name`, path, enclosing symbol name or None) per marker.
fn marker_tuples(envelope: &Value) -> Vec<(String, String, String, Option<String>)> {
    envelope["markers"]
        .as_array()
        .expect("markers array")
        .iter()
        .map(|m| {
            (
                m["category"].as_str().expect("category").to_owned(),
                m["macro_name"].as_str().expect("macro_name").to_owned(),
                m["repo_relative_path"].as_str().expect("path").to_owned(),
                m["enclosing_symbol"]["name"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Classification: 100% of risk sites, 0% of benign
// ---------------------------------------------------------------------------

#[test]
fn risk_markers_returns_only_risk_classified_markers() {
    let (_temp, graph) = fixture_scanned();
    let envelope = run_lane(&graph, &[]);

    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "risk-markers");
    assert_eq!(
        envelope["risk_macro_set"],
        serde_json::json!(["panic!", "todo!", "unimplemented!", "unreachable!"])
    );
    assert_eq!(envelope["counts"]["total"], 5);
    assert_eq!(envelope["counts"]["stub"], 4);
    assert_eq!(envelope["counts"]["panic"], 1);
    assert!(envelope.get("empty_reason").is_none_or(Value::is_null));

    let tuples = marker_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "stub".to_owned(),
                "todo!".to_owned(),
                "src/lib.rs".to_owned(),
                Some("finish".to_owned())
            ),
            (
                "panic".to_owned(),
                "panic!".to_owned(),
                "src/lib.rs".to_owned(),
                Some("abort_early".to_owned())
            ),
            (
                "stub".to_owned(),
                "unimplemented!".to_owned(),
                "src/lib.rs".to_owned(),
                Some("decide".to_owned())
            ),
            (
                "stub".to_owned(),
                "unreachable!".to_owned(),
                "src/lib.rs".to_owned(),
                Some("decide".to_owned())
            ),
            (
                "stub".to_owned(),
                "todo!".to_owned(),
                "src/toplevel.rs".to_owned(),
                None
            ),
        ]
    );

    // Zero false-risk: no benign macro name may appear in the inventory.
    let names: Vec<&str> = envelope["markers"]
        .as_array()
        .expect("markers")
        .iter()
        .map(|m| m["macro_name"].as_str().expect("macro_name"))
        .collect();
    for benign in ["println!", "vec!", "format!"] {
        assert!(
            !names.contains(&benign),
            "benign macro {benign} must never appear as a risk marker"
        );
    }

    // Every row carries the citable handles the acceptance criteria require.
    for marker in envelope["markers"].as_array().expect("markers") {
        assert!(
            marker["record_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("codegraph:")),
            "record_id must be a stable code-graph handle: {marker}"
        );
        assert_eq!(marker["kind"], "Diagnostic");
        assert!(marker["span"]["start_byte"].as_u64().is_some());
        assert!(marker["span"]["start_line"].as_u64().is_some());
        assert_eq!(marker["trust"], "source_fact");
        assert!(
            marker.get("enclosing_symbol").is_some(),
            "enclosing_symbol must always be present (explicit null at top level): {marker}"
        );
    }
    // The module-top-level marker carries an explicit null, never an omission.
    let top = envelope["markers"]
        .as_array()
        .expect("markers")
        .iter()
        .find(|m| m["repo_relative_path"] == "src/toplevel.rs")
        .expect("top-level marker");
    assert_eq!(top["enclosing_symbol"], Value::Null);

    // Corpus disclosure rides the envelope (issue #456 contract).
    assert_eq!(envelope["corpus_mode"], "head_anchored");
    assert_eq!(envelope["corpus_mode_source"], "default");
    assert!(
        envelope["corpus_disclaimer"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
}

#[test]
fn risk_markers_never_mixes_categories() {
    let (_temp, graph) = fixture_scanned();
    let envelope = run_lane(&graph, &[]);
    for marker in envelope["markers"].as_array().expect("markers") {
        let (category, name) = (
            marker["category"].as_str().expect("category"),
            marker["macro_name"].as_str().expect("macro_name"),
        );
        match name {
            "todo!" | "unimplemented!" | "unreachable!" => assert_eq!(category, "stub"),
            "panic!" => assert_eq!(category, "panic"),
            other => panic!("unexpected macro {other} in the risk inventory"),
        }
    }
}

// ---------------------------------------------------------------------------
// Scoping: path prefix, repo selector, empty vs not-found
// ---------------------------------------------------------------------------

#[test]
fn risk_markers_path_prefix_scopes_to_subsystem() {
    let (_temp, graph) = fixture_scanned();

    let scoped = run_lane(&graph, &["--path", "src/empty"]);
    assert_eq!(scoped["counts"]["total"], 0);
    assert_eq!(scoped["empty_reason"], "no_markers_in_scope");
    assert_eq!(scoped["path_prefix"], "src/empty");

    // Trailing-slash form resolves identically (subsystem contract).
    let slashed = run_lane(&graph, &["--path", "src/empty/"]);
    assert_eq!(slashed["counts"]["total"], 0);

    // Segment awareness: `src/li` is not a path-segment prefix of
    // `src/lib.rs`, so it matches nothing and is a documented scope error —
    // no sibling-path bleed.
    let output = run_lane_raw(&graph, &["--path", "src/li"]);
    assert_eq!(output.status.code(), Some(2));
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("machine-readable envelope");
    assert_eq!(envelope["error"]["code"], "scope_not_found");

    // An exact file path scopes to that file's markers.
    let file_scoped = run_lane(&graph, &["--path", "src/lib.rs"]);
    assert_eq!(file_scoped["counts"]["total"], 4);
    assert_eq!(file_scoped["counts"]["stub"], 3);
    assert_eq!(file_scoped["counts"]["panic"], 1);
}

#[test]
fn risk_markers_out_of_store_prefix_is_a_diagnostic_not_silence() {
    let (_temp, graph) = fixture_scanned();
    let output = run_lane_raw(&graph, &["--path", "src/does-not-exist"]);
    assert_eq!(output.status.code(), Some(2));
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("machine-readable envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "scope_not_found");
}

#[test]
fn risk_markers_unknown_repo_selector_exits_nonzero_machine_readable() {
    let (_temp, graph) = fixture_scanned();
    let output = run_lane_raw(&graph, &["--repo", "no-such-repo"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    let diag: Value = serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(diag["code"], "unknown_repository_selector");
    assert_eq!(diag["selector"], "no-such-repo");
}

#[test]
fn risk_markers_known_repo_selector_scopes_through_the_file() {
    // The scanner wires no containment edges for `Diagnostic` nodes, so the
    // lane attributes a marker to the selected repository through its file.
    // A known selector must return the fixture's markers — never a false
    // "clean" — each carrying the selected repository's stable ID.
    let (_temp, graph) = fixture_scanned();
    let envelope = run_lane(&graph, &["--repo", "risk-markers-fixture"]);
    assert_eq!(envelope["counts"]["total"], 5);
    let repo_ids: Vec<&str> = envelope["markers"]
        .as_array()
        .expect("markers")
        .iter()
        .map(|m| m["repository_id"].as_str().expect("repository_id"))
        .collect();
    assert!(
        repo_ids.iter().all(|id| id.starts_with("codegraph:")),
        "every row should carry the selected repository's stable ID: {repo_ids:?}"
    );
    assert!(
        envelope["markers"]
            .as_array()
            .expect("markers")
            .iter()
            .all(|m| m["repository"].as_str() == Some("risk-markers-fixture")),
        "every row should carry the human-usable repository handle"
    );
}

#[test]
fn risk_markers_benign_only_scope_is_explicitly_clean() {
    let (_temp, graph) = fixture_scanned();
    // `src/benign.rs` holds println!/vec! diagnostics but zero risk markers:
    // the lane must answer "clean", not "not found".
    let envelope = run_lane(&graph, &["--path", "src/benign.rs"]);
    assert_eq!(envelope["counts"]["total"], 0);
    assert_eq!(envelope["empty_reason"], "no_markers_in_scope");
}

// ---------------------------------------------------------------------------
// Temporal selector
// ---------------------------------------------------------------------------

#[test]
fn risk_markers_at_commit_pins_the_inventory_to_valid_time() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let first = commit(&repo, "no risk yet", "2026-01-01T00:00:00Z");

    write(
        &repo,
        "src/lib.rs",
        "pub fn calm() -> u32 {\n    todo!(\"introduce a stub\")\n}\n",
    );
    let second = commit(&repo, "introduce todo", "2026-01-02T00:00:00Z");

    let graph_path = temp.path().join("history.graph.jsonl");
    let jsonl = scan_repository_history_with_override(&repo, Some("risk-markers-history"))
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    // Pinned before the introduction: the marker must not appear.
    let at_first = run_lane(&graph_path, &["--at", &first]);
    assert_eq!(at_first["counts"]["total"], 0);
    assert_eq!(at_first["empty_reason"], "no_markers_in_scope");
    assert_eq!(at_first["at_commit"], Value::String(first));

    // Pinned at the introducing commit: exactly one stub marker.
    let at_second = run_lane(&graph_path, &["--at", &second]);
    assert_eq!(at_second["counts"]["total"], 1);
    assert_eq!(at_second["counts"]["stub"], 1);
    let tuples = marker_tuples(&at_second);
    assert_eq!(tuples[0].1, "todo!");
    assert_eq!(tuples[0].3, Some("calm".to_owned()));
    assert_eq!(
        at_second["markers"][0]["git_commit"].as_str(),
        Some(second.as_str())
    );

    // A commit the store has never seen is a documented diagnostic (exit 2),
    // never a silent empty result.
    egregore()
        .args(["query", "risk-markers", "--graph"])
        .arg(&graph_path)
        .args(["--at", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"unknown_commit\""));
}

// ---------------------------------------------------------------------------
// Text format
// ---------------------------------------------------------------------------

#[test]
fn risk_markers_text_format_is_one_line_per_marker() {
    let (_temp, graph) = fixture_scanned();
    let output = run_lane_raw(&graph, &["--format", "text"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "one line per marker: {stdout}");
    assert!(
        lines[0].contains("stub todo!"),
        "first line should cite the stub: {}",
        lines[0]
    );
    assert!(
        lines.iter().any(|l| l.contains("panic panic!")),
        "a panic line must be present: {stdout}"
    );
    assert!(
        lines.iter().any(|l| l.contains("(top-level)")),
        "the top-level marker must render its owner: {stdout}"
    );
}

#[test]
fn risk_markers_text_format_marks_clean_scope() {
    let (_temp, graph) = fixture_scanned();
    let output = run_lane_raw(&graph, &["--path", "src/empty", "--format", "text"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        stdout.contains("no_markers_in_scope"),
        "clean scope must be explicit: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Determinism + read-only guarantees
// ---------------------------------------------------------------------------

#[test]
fn risk_markers_output_is_byte_identical_across_five_runs() {
    let (_temp, graph) = fixture_scanned();
    let first = run_lane_raw(&graph, &[]).stdout;
    for i in 0..4 {
        let next = run_lane_raw(&graph, &[]).stdout;
        assert_eq!(first, next, "run {} diverged from the first run", i + 2);
    }
}

#[test]
fn risk_markers_query_is_read_only() {
    let (_temp, graph) = fixture_scanned();
    let before = fs::read(&graph).expect("graph should read");
    for _ in 0..3 {
        run_lane(&graph, &[]);
        run_lane(&graph, &["--path", "src"]);
    }
    let after = fs::read(&graph).expect("graph should read");
    assert_eq!(before, after, "the lane must not modify the graph file");
}
