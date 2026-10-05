//! Integration tests for `eg query diagnostics` (issue #246).
//!
//! The lane lists persisted extractor-coverage-gap `Diagnostic` graph nodes —
//! the extractor's self-declared blind spots (unsupported macro invocations,
//! unresolved call/dispatch stubs) — as citable rows, one NDJSON object per
//! gap, with no scan or recompute. It never reclassifies source-authored
//! markers (`DebtMarker` TODO/FIXME notes, `PanicRiskSite` unwrap/expect
//! sites), which belong to other lanes.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{GraphRecord, NodeKind, scan_repository_at_with_override};
use assert_cmd::Command as CargoCommand;
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

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    git(
        repo,
        [
            "-c",
            "user.email=codegraph@example.invalid",
            "-c",
            "user.name=Codegraph Test",
            "commit",
            "-m",
            message,
            "--date",
            date,
        ],
    );
}

const GAP_RS: &str = "fn main() {\n    // TODO: stopgap until the real parser lands\n    let value: Option<u32> = None;\n    let forced = value.unwrap();\n    println!(\"forced={forced}\");\n}\n";
const CLEAN_RS: &str = "pub fn double(value: u32) -> u32 {\n    value.saturating_mul(2)\n}\n";

/// Scans a fixture repo holding one gap-bearing file (`src/gap.rs`) and one
/// clean file (`src/clean.rs`). Returns (`TempDir`, `repo`, `graph_path`).
fn fixture_scanned(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/gap.rs", GAP_RS);
    write(&repo, "src/clean.rs", CLEAN_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(&repo, "2026-01-01T00:00:00Z", Some(name))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, repo, graph_path)
}

fn graph_records(graph: &Path) -> Vec<GraphRecord> {
    let jsonl = fs::read_to_string(graph).expect("graph should read");
    aletheia_egregore::adapters::records_from_jsonl(&jsonl).expect("graph should parse")
}

/// Every persisted extractor-gap `Diagnostic` node id (the lane's closed set:
/// `Diagnostic` kind with a file path AND a span).
fn persisted_gap_ids(graph: &Path) -> Vec<String> {
    graph_records(graph)
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Node {
                id,
                kind: NodeKind::Diagnostic,
                repo_relative_path: Some(_),
                span: Some(_),
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn run_query(graph: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = egregore();
    cmd.args(["query", "diagnostics", "--graph"]).arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("query should execute")
}

fn stdout_lines(output: &std::process::Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "query failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout should be utf-8");
    stdout.lines().map(str::to_owned).collect()
}

fn parse_rows(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("each stdout line should be one JSON object"))
        .collect()
}

// ---------------------------------------------------------------------------
// Lane correctness
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_returns_every_persisted_gap_and_nothing_else() {
    let (_temp, repo, graph) = fixture_scanned("diagnostics-fixture");

    // Sanity: the fixture really does hold source-authored markers that must
    // NOT be reclassified as extractor gaps.
    let records = graph_records(&graph);
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::DebtMarker,
                ..
            }
        )),
        "fixture should persist a DebtMarker (TODO comment)"
    );
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::PanicRiskSite,
                ..
            }
        )),
        "fixture should persist a PanicRiskSite (.unwrap())"
    );

    let expected_ids: std::collections::BTreeSet<String> =
        persisted_gap_ids(&graph).into_iter().collect();
    assert!(
        !expected_ids.is_empty(),
        "fixture should persist at least one extractor-gap Diagnostic"
    );

    let lines = stdout_lines(&run_query(&graph, &[]));
    let rows = parse_rows(&lines);

    // 100% of persisted gap markers, 0 false absences.
    let row_ids: std::collections::BTreeSet<String> = rows
        .iter()
        .map(|row| {
            row["record_id"]
                .as_str()
                .expect("row should carry record_id")
                .to_owned()
        })
        .collect();
    assert_eq!(
        row_ids, expected_ids,
        "lane must return exactly the persisted gap markers"
    );

    for row in &rows {
        assert_eq!(row["kind"], "Diagnostic");
        assert!(
            row["name"].as_str().is_some_and(|name| !name.is_empty()),
            "row should carry the diagnostic subject"
        );
        assert!(
            row["summary"]
                .as_str()
                .is_some_and(|summary| !summary.is_empty()),
            "row should carry the human summary"
        );
        let path = row["repo_relative_path"]
            .as_str()
            .expect("row should carry repo_relative_path");
        let span = &row["span"];
        for key in ["start_byte", "end_byte", "start_line", "end_line"] {
            assert!(span[key].as_u64().is_some(), "row span should carry {key}");
        }
        // The span must land on the flagged source region.
        let source = fs::read(repo.join(path)).expect("flagged file should read");
        let start = usize::try_from(span["start_byte"].as_u64().expect("start_byte"))
            .expect("start_byte fits in usize");
        let end = usize::try_from(span["end_byte"].as_u64().expect("end_byte"))
            .expect("end_byte fits in usize");
        assert!(
            start < end && end <= source.len(),
            "span should slice the file"
        );
        assert!(
            !source[start..end].is_empty(),
            "span should cover the flagged region"
        );
    }

    // The unsupported-macro gap is the documented example shape.
    assert!(
        rows.iter().any(|row| row["summary"]
            .as_str()
            .is_some_and(|s| s.starts_with("unsupported macro invocation"))),
        "lane should surface the unsupported-macro-invocation gap"
    );
}

#[test]
fn diagnostics_output_is_byte_stable_across_runs() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-stable");
    let first = run_query(&graph, &[]).stdout;
    let second = run_query(&graph, &[]).stdout;
    assert_eq!(
        first, second,
        "repeated runs must be byte-for-byte identical"
    );
    // Deterministic ordering: rows are sorted by (path, start_byte, id).
    let lines: Vec<String> = String::from_utf8(first)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect();
    let rows = parse_rows(&lines);
    let keys: Vec<(String, u64, String)> = rows
        .iter()
        .map(|row| {
            (
                row["repo_relative_path"].as_str().expect("path").to_owned(),
                row["span"]["start_byte"].as_u64().expect("start_byte"),
                row["record_id"].as_str().expect("record_id").to_owned(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "rows must be in canonical order");
}

#[test]
fn diagnostics_file_scope_filters_to_one_path() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-file");
    let all = parse_rows(&stdout_lines(&run_query(&graph, &[])));
    let scoped = parse_rows(&stdout_lines(&run_query(&graph, &["--file", "src/gap.rs"])));
    let expected: Vec<&Value> = all
        .iter()
        .filter(|row| row["repo_relative_path"] == "src/gap.rs")
        .collect();
    assert_eq!(
        scoped.len(),
        expected.len(),
        "--file should return exactly the rows for that path"
    );
    assert!(
        scoped
            .iter()
            .all(|row| row["repo_relative_path"] == "src/gap.rs"),
        "every scoped row should cite the requested file"
    );
    // --file is an order-preserving subsequence of the unscoped answer.
    let all_ids: Vec<&str> = expected
        .iter()
        .map(|row| row["record_id"].as_str().expect("record_id"))
        .collect();
    let scoped_ids: Vec<&str> = scoped
        .iter()
        .map(|row| row["record_id"].as_str().expect("record_id"))
        .collect();
    assert_eq!(scoped_ids, all_ids);
}

#[test]
fn diagnostics_clean_file_scope_is_an_explicit_empty_result() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-clean-file");
    let output = run_query(&graph, &["--file", "src/clean.rs"]);
    assert!(
        output.status.success(),
        "a file with zero gaps is scope-clean, not an error\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = stdout_lines(&output);
    assert_eq!(
        lines.len(),
        1,
        "clean scope should emit one explicit marker line"
    );
    let marker: Value = serde_json::from_str(&lines[0]).expect("marker should parse");
    assert_eq!(marker["ok"], true);
    assert_eq!(marker["lane"], "diagnostics");
    assert_eq!(marker["diagnostics"], serde_json::json!([]));
    assert_eq!(marker["empty_reason"], "no_gaps_in_scope");
}

#[test]
fn diagnostics_unknown_file_is_a_machine_readable_scope_error() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-unknown-file");
    let output = run_query(&graph, &["--file", "src/missing.rs"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a --file naming nothing in the store is a scope error, not clean"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("machine-readable envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "unknown_file");
}

#[test]
fn diagnostics_unknown_repo_selector_exits_nonzero_machine_readable() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-repo");
    let output = run_query(&graph, &["--repo", "no-such-repo"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    let diag: Value = serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(diag["code"], "unknown_repository_selector");
    assert_eq!(diag["selector"], "no-such-repo");
    assert!(
        output.stdout.is_empty(),
        "no partial rows should reach stdout on a selector error"
    );
}

#[test]
fn diagnostics_repo_scope_restricts_to_one_repository() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (_t1, _r1, graph_a) = {
        let repo = temp.path().join("a");
        fs::create_dir_all(&repo).expect("repo dir");
        init_git(&repo);
        write(&repo, "src/a.rs", "fn a() {\n    println!(\"a\");\n}\n");
        commit(&repo, "seed a", "2026-01-01T00:00:00Z");
        let jsonl =
            scan_repository_at_with_override(&repo, "2026-01-01T00:00:00Z", Some("diag-repo-a"))
                .expect("repo a should scan")
                .to_jsonl()
                .expect("serialize");
        let path = temp.path().join("a.jsonl");
        fs::write(&path, &jsonl).expect("write");
        (tempfile::tempdir().expect("unused"), repo, path)
    };
    let repo_b = temp.path().join("b");
    fs::create_dir_all(&repo_b).expect("repo dir");
    init_git(&repo_b);
    write(&repo_b, "src/b.rs", "fn b() {\n    eprintln!(\"b\");\n}\n");
    commit(&repo_b, "seed b", "2026-01-01T00:00:00Z");
    let jsonl_b =
        scan_repository_at_with_override(&repo_b, "2026-01-01T00:00:00Z", Some("diag-repo-b"))
            .expect("repo b should scan")
            .to_jsonl()
            .expect("serialize");
    let graph_b = temp.path().join("b.jsonl");
    fs::write(&graph_b, &jsonl_b).expect("write");

    // One shared store holding both repositories.
    let mut both = fs::read_to_string(&graph_a).expect("read a");
    both.push_str(&fs::read_to_string(&graph_b).expect("read b"));
    let both_path = temp.path().join("both.jsonl");
    fs::write(&both_path, &both).expect("write");

    let all = parse_rows(&stdout_lines(&run_query(&both_path, &[])));
    assert!(!all.is_empty());
    let repo_id_a = all
        .iter()
        .find(|row| row["repo_relative_path"] == "src/a.rs")
        .expect("row from repo a")["repository_id"]
        .as_str()
        .expect("repository_id")
        .to_owned();

    let scoped = parse_rows(&stdout_lines(&run_query(
        &both_path,
        &["--repo", &repo_id_a],
    )));
    assert!(
        !scoped.is_empty(),
        "--repo should keep the selected repository's gaps"
    );
    assert!(
        scoped.iter().all(|row| row["repository_id"] == repo_id_a),
        "every scoped row should belong to the selected repository"
    );
    assert!(
        scoped.len() < all.len(),
        "scoping should drop the other repository's gaps"
    );
}

#[test]
fn diagnostics_text_format_is_human_readable() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-text");
    let output = run_query(&graph, &["--format", "text"]);
    let lines = stdout_lines(&output);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            line.contains("src/gap.rs:"),
            "text row should cite path:line, got: {line}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|line| line.contains("unsupported macro invocation println!")),
        "text rows should carry the human summary"
    );
}

#[test]
fn diagnostics_clean_scope_text_format_names_the_empty_reason() {
    let (_temp, _repo, graph) = fixture_scanned("diagnostics-clean-text");
    let output = run_query(&graph, &["--file", "src/clean.rs", "--format", "text"]);
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("no_gaps_in_scope"),
        "text empty marker should name the reason, got: {}",
        lines[0]
    );
}
