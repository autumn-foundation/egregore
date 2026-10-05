//! Integration tests for `eg query suppressions` (issue #227).
//!
//! The lane inventories the scanned repo's own `#[allow(...)]` /
//! `#![allow(...)]` lint suppressions as detected by the Tree-sitter
//! extractor over the attribute AST: genuine allow attributes only (never
//! `#[allow(` text inside line comments, doc comments, block comments, or
//! string literals), each carrying the sorted lint-name list, the closed
//! scope (`item` / `module` / `crate`), the adjacent justification-comment
//! signal, a repo-relative file/span handle, and the enclosing symbol handle
//! when one exists, plus aggregate counts.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{GraphRecord, NodeKind, scan_repository_at_with_override};
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

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) {
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
}

/// Genuine allow attributes of all three closed scopes plus every decoy class
/// from the issue #227 acceptance criteria: `#[allow(` text inside a line
/// comment, a doc comment, a block comment, and a string literal. The
/// `#[cfg_attr(test, allow(dead_code))]`, bare `#[allow]`, and empty
/// `#[allow()]` forms prove the extractor only accepts the attribute's own
/// `allow` name with a non-empty lint list.
const LIB_RS: &str = r##"#![allow(dead_code)]

// Line-comment decoy: #[allow(dead_code)] is prose here.
/// Doc-comment decoy: #[allow(dead_code)] is prose here.
/* Block-comment decoy: #[allow(dead_code)] is prose here. */
pub fn decoys() -> u32 {
    let s = "#[allow(dead_code)]";
    let _ = s;
    1
}

// Preceding justification: this allow is load-bearing for the FFI shim.
#[allow(clippy::too_many_arguments)]
pub fn justified(a: u32, b: u32, c: u32, d: u32, e: u32, f: u32, g: u32, h: u32) -> u32 {
    a + b + c + d + e + f + g + h
}

#[allow(dead_code, unused_variables)] // trailing justification: silences the scratch binding
pub fn multi() -> u32 {
    let unused = 1u32;
    2
}

#[allow(dead_code)]

pub fn blank_separated() -> u32 {
    3
}

mod inner {
    #![allow(unused_imports)]

    pub fn nested() -> u32 {
        4
    }
}

#[cfg_attr(test, allow(dead_code))]
pub fn cfg_attr_excluded() -> u32 {
    5
}

#[allow]
pub fn bare_excluded() -> u32 {
    6
}

#[allow()]
pub fn empty_excluded() -> u32 {
    7
}
"##;

const CLEAN_RS: &str = "pub fn clean() -> u32 {\n    7\n}\n";

/// Builds the seeded fixture checkout, scans it deterministically, and writes
/// the JSONL graph. Returns (`TempDir`, `graph_path`).
fn fixture_scanned() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write(&repo, "src/lib.rs", LIB_RS);
    write(&repo, "src/empty/clean.rs", CLEAN_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("suppressions-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, graph_path)
}

fn run_lane(graph: &Path, extra: &[&str]) -> (Value, String) {
    let mut cmd = egregore();
    cmd.args(["query", "suppressions", "--graph"]).arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("query should execute");
    assert!(
        output.status.success(),
        "query failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let value: Value = serde_json::from_str(&stdout).expect("stdout should be one JSON envelope");
    (value, stdout)
}

/// (`scope`, `lints`, `has_justification`, enclosing symbol name or `None`).
fn suppression_tuples(envelope: &Value) -> Vec<(String, Vec<String>, bool, Option<String>)> {
    envelope["suppressions"]
        .as_array()
        .expect("suppressions array")
        .iter()
        .map(|row| {
            let enclosing = if row["enclosing_symbol"].is_null() {
                None
            } else {
                Some(
                    row["enclosing_symbol"]["name"]
                        .as_str()
                        .expect("enclosing symbol name")
                        .to_owned(),
                )
            };
            (
                row["scope"].as_str().expect("scope").to_owned(),
                row["lints"]
                    .as_array()
                    .expect("lints")
                    .iter()
                    .map(|l| l.as_str().expect("lint").to_owned())
                    .collect(),
                row["has_justification"]
                    .as_bool()
                    .expect("has_justification"),
                enclosing,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Extraction + lane correctness
// ---------------------------------------------------------------------------

#[test]
fn suppressions_returns_real_suppressions_and_never_decoys() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &[]);

    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "suppressions");
    assert!(
        envelope["disclaimer"]
            .as_str()
            .expect("disclaimer")
            .contains("never"),
        "results must state they are an inventory, not a verdict on whether \
         a suppression is justified"
    );

    let tuples = suppression_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "crate".to_owned(),
                vec!["dead_code".to_owned()],
                false,
                None,
            ),
            (
                "item".to_owned(),
                vec!["clippy::too_many_arguments".to_owned()],
                true,
                Some("justified".to_owned()),
            ),
            (
                "item".to_owned(),
                vec!["dead_code".to_owned(), "unused_variables".to_owned()],
                true,
                Some("multi".to_owned()),
            ),
            (
                "item".to_owned(),
                vec!["dead_code".to_owned()],
                false,
                Some("blank_separated".to_owned()),
            ),
            (
                "module".to_owned(),
                vec!["unused_imports".to_owned()],
                false,
                None,
            ),
        ],
        "the lane must return exactly the genuine allow attributes: no \
         comment, doc-comment, block-comment, or string-literal decoys; no \
         cfg_attr, bare, or empty forms"
    );

    assert_eq!(
        envelope["counts"],
        serde_json::json!({
            "total": 5,
            "item": 3,
            "module": 1,
            "crate": 1,
            "justified": 2,
            "unjustified": 3,
        }),
        "counts must break down per scope and justification signal"
    );

    for row in envelope["suppressions"].as_array().expect("suppressions") {
        assert_eq!(row["kind"], "LintSuppression");
        assert_eq!(row["trust"], "source_fact");
        assert!(
            row["record_id"]
                .as_str()
                .expect("record_id")
                .starts_with("codegraph:v"),
            "each suppression must carry a stable record ID"
        );
        assert_eq!(row["language"], "rust");
        let span = &row["span"];
        assert!(span["start_line"].as_u64().expect("start_line") >= 1);
        assert!(
            span["end_byte"].as_u64().expect("end_byte")
                > span["start_byte"].as_u64().expect("start_byte")
        );
    }

    // The line-comment decoy is line 3, the doc-comment decoy line 4, the
    // block-comment decoy line 5, and the string-literal decoy line 7 of
    // src/lib.rs; no returned span may start there.
    for row in envelope["suppressions"].as_array().expect("suppressions") {
        if row["repo_relative_path"] == "src/lib.rs" {
            let start_line = row["span"]["start_line"].as_u64().expect("line");
            assert!(
                !matches!(start_line, 3 | 4 | 5 | 7),
                "decoy line {start_line} must never be returned"
            );
        }
    }
}

#[test]
fn suppressions_scan_emits_suppression_records_with_payload() {
    let (_temp, graph) = fixture_scanned();
    let jsonl = fs::read_to_string(&graph).expect("graph should read");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let suppressions: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::LintSuppression,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        suppressions.len(),
        5,
        "scan must emit one record per allow attribute"
    );
    for record in suppressions {
        let GraphRecord::Node {
            name,
            language,
            span,
            repo_relative_path,
            lint_suppression,
            ..
        } = record
        else {
            unreachable!()
        };
        assert!(matches!(name.as_deref(), Some("item" | "module" | "crate")));
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(span.is_some(), "each suppression must carry a source span");
        assert!(repo_relative_path.is_some());
        let facts = lint_suppression
            .as_ref()
            .expect("each suppression must carry its facts payload");
        assert!(!facts.lints.is_empty());
        assert_eq!(facts.scope.as_str(), name.as_deref().expect("scope name"));
    }
}

#[test]
fn suppressions_graph_passes_referential_integrity_validation() {
    // The `CONTAINS File -> LintSuppression` edges emitted by the extractor
    // must satisfy the issue #103 pre-ingest referential-integrity gate.
    let (_temp, graph) = fixture_scanned();
    egregore().arg("validate").arg(&graph).assert().success();
}

// ---------------------------------------------------------------------------
// Scope: prefix filter, empty-vs-not-found honesty, --repo
// ---------------------------------------------------------------------------

#[test]
fn suppressions_prefix_scopes_results_segment_aware() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &["--path", "src/empty"]);
    assert_eq!(envelope["suppressions"], serde_json::json!([]));
    assert_eq!(envelope["counts"]["total"], 0);
    assert_eq!(envelope["empty_reason"], "no_suppressions_in_scope");
    assert_eq!(envelope["path_prefix"], "src/empty");
}

#[test]
fn suppressions_empty_scope_is_distinguished_from_scope_not_found() {
    let (_temp, graph) = fixture_scanned();

    // Scope not present in the store: scope_not_found, exit 2 — never a
    // silent empty result (issue #196 honesty contract).
    egregore()
        .args(["query", "suppressions", "--graph"])
        .arg(&graph)
        .args(["--path", "src/nonexistent"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Sibling-prefix bleed: `src/emp` must not match `src/empty`.
    egregore()
        .args(["query", "suppressions", "--graph"])
        .arg(&graph)
        .args(["--path", "src/emp"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Malformed prefix: exit 1 with a machine-readable diagnostic.
    egregore()
        .args(["query", "suppressions", "--graph"])
        .arg(&graph)
        .args(["--path", "/"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"malformed_prefix\""));
}

#[test]
fn suppressions_repo_selector_scopes_and_rejects_unknown() {
    let (_temp, graph) = fixture_scanned();

    // The override identity resolves as a repo selector.
    let (envelope, _) = run_lane(&graph, &["--repo", "suppressions-fixture"]);
    assert_eq!(envelope["counts"]["total"], 5);

    // An unknown selector is a documented diagnostic, never a silent empty.
    egregore()
        .args(["query", "suppressions", "--graph"])
        .arg(&graph)
        .args(["--repo", "no-such-repo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

#[test]
fn suppressions_text_format_lists_one_line_per_suppression() {
    let (_temp, graph) = fixture_scanned();
    let output = egregore()
        .args(["query", "suppressions", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .output()
        .expect("query should execute");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "one line per suppression: {stdout}");
    assert!(
        lines[0].starts_with("crate [dead_code] (unjustified) @ src/lib.rs:1 in (top-level)"),
        "unexpected first line: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("item [clippy::too_many_arguments] (justified)")
            && lines[1].contains("in justified"),
        "unexpected second line: {}",
        lines[1]
    );
}

#[test]
fn suppressions_lane_manifest_entry_is_structural_source_derived_and_citable() {
    let output = egregore()
        .args(["query", "lanes", "--format", "json"])
        .output()
        .expect("lanes should execute");
    assert!(output.status.success());
    let manifest: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be a lane manifest");
    let lane = manifest["lanes"]
        .as_array()
        .expect("lanes array")
        .iter()
        .find(|l| l["id"] == "suppressions")
        .expect("suppressions lane must be registered");
    assert_eq!(lane["store_mode"], "structural");
    assert_eq!(lane["trust_classes"], serde_json::json!(["source_derived"]));
    assert_eq!(lane["citable_handles"], true);
    assert_eq!(lane["freshness"], serde_json::json!(["ingest_snapshot"]));
    assert!(
        lane["purpose"]
            .as_str()
            .expect("purpose")
            .contains("lint suppressions"),
        "the manifest purpose must describe the lane"
    );
}

// ---------------------------------------------------------------------------
// Determinism + read-only guarantees
// ---------------------------------------------------------------------------

#[test]
fn suppressions_output_is_byte_identical_across_five_runs() {
    let (_temp, graph) = fixture_scanned();
    let (_, first) = run_lane(&graph, &[]);
    for _ in 0..4 {
        let (_, next) = run_lane(&graph, &[]);
        assert_eq!(
            first, next,
            "re-running the identical query against an unchanged store must \
             yield byte-identical output"
        );
    }
}

#[test]
fn suppressions_query_is_read_only() {
    let (temp, graph) = fixture_scanned();
    let bytes_before = fs::read(&graph).expect("graph should read");
    let listing_before = dir_listing(temp.path());

    let (_, _) = run_lane(&graph, &[]);

    assert_eq!(
        fs::read(&graph).expect("graph should read"),
        bytes_before,
        "querying must not modify the store"
    );
    assert_eq!(
        dir_listing(temp.path()),
        listing_before,
        "querying must not create or delete any files"
    );
}

fn dir_listing(root: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("dir should read") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            entries.push(path.display().to_string());
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    entries.sort();
    entries
}
