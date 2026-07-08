//! Integration tests for `eg query debt-markers` (issue #218).
//!
//! The lane inventories human-authored debt-comment markers (`TODO`,
//! `FIXME`, `HACK`, `XXX`) detected inside Tree-sitter comment nodes: real
//! comment markers only (never string-literal or identifier-substring
//! decoys), each carrying a closed machine-readable category, the trimmed
//! single-line note text, a repo-relative file/span handle, and the
//! enclosing symbol handle when one exists (explicit `null` at top level).
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, NodeKind, scan_repository_at_with_override, scan_repository_history_with_override,
};
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

/// Genuine markers in line, block, and doc comments across all four
/// categories, plus every decoy class from the issue #218 acceptance
/// criteria: a marker token inside a string literal, marker substrings
/// inside identifiers, a marker substring inside a longer comment word, and
/// benign comments with no marker at all.
const LIB_RS: &str = r#"/// Doc-marker: HACK: bypasses validation cache
pub fn parse_port(input: &str) -> u16 {
    // TODO: wire retry logic
    let decoy = "TODO: not a marker";
    let _ = decoy;
    let todoist = 1; // benign comment, sizes XXXL never match
    let _ = todoist;
    input.trim().parse().unwrap_or(0)
}

pub fn read_config(path: &str) -> String {
    /* FIXME handle empty input */
    let fixmeup = path.len(); /* benign block comment */
    let _ = fixmeup;
    path.to_owned()
}

pub fn reorder() -> u32 {
    // xxx revisit ordering
    2
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
    write(&repo, "src/empty/clean.rs", CLEAN_RS);
    commit(&repo, "seed fixture", "2026-01-01T00:00:00Z");

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_repository_at_with_override(
        &repo,
        "2026-01-01T00:00:00Z",
        Some("debt-markers-fixture"),
    )
    .expect("fixture repo should scan")
    .to_jsonl()
    .expect("graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");
    (temp, graph_path)
}

fn run_lane(graph: &Path, extra: &[&str]) -> (Value, String) {
    let mut cmd = egregore();
    cmd.args(["query", "debt-markers", "--graph"]).arg(graph);
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

/// (`category`, `note`, `repo_relative_path`, enclosing symbol name or `None`).
fn marker_tuples(envelope: &Value) -> Vec<(String, String, String, Option<String>)> {
    envelope["markers"]
        .as_array()
        .expect("markers array")
        .iter()
        .map(|marker| {
            let enclosing = if marker["enclosing_symbol"].is_null() {
                None
            } else {
                Some(
                    marker["enclosing_symbol"]["name"]
                        .as_str()
                        .expect("enclosing symbol name")
                        .to_owned(),
                )
            };
            (
                marker["category"].as_str().expect("category").to_owned(),
                marker["note"].as_str().expect("note").to_owned(),
                marker["repo_relative_path"]
                    .as_str()
                    .expect("repo_relative_path")
                    .to_owned(),
                enclosing,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Extraction + lane correctness
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_returns_real_markers_and_never_decoys() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, stdout) = run_lane(&graph, &[]);

    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["lane"], "debt_markers");
    assert_eq!(
        envelope["marker_set"],
        serde_json::json!(["fixme", "hack", "todo", "xxx"]),
        "the recognized marker set is closed for this slice"
    );
    assert!(
        envelope["disclaimer"]
            .as_str()
            .expect("disclaimer")
            .contains("advisory"),
        "results must be labeled advisory triage, never a verification claim"
    );

    let tuples = marker_tuples(&envelope);
    assert_eq!(
        tuples,
        vec![
            (
                "hack".to_owned(),
                "bypasses validation cache".to_owned(),
                "src/lib.rs".to_owned(),
                None,
            ),
            (
                "todo".to_owned(),
                "wire retry logic".to_owned(),
                "src/lib.rs".to_owned(),
                Some("parse_port".to_owned()),
            ),
            (
                "fixme".to_owned(),
                "handle empty input".to_owned(),
                "src/lib.rs".to_owned(),
                Some("read_config".to_owned()),
            ),
            (
                "xxx".to_owned(),
                "revisit ordering".to_owned(),
                "src/lib.rs".to_owned(),
                Some("reorder".to_owned()),
            ),
        ],
        "the lane must return exactly the genuine comment markers: no \
         string-literal, identifier-substring, or longer-word decoys, and \
         benign comments must never match"
    );

    assert_eq!(envelope["counts"]["total"], 4);
    assert_eq!(envelope["counts"]["todo"], 1);
    assert_eq!(envelope["counts"]["fixme"], 1);
    assert_eq!(envelope["counts"]["hack"], 1);
    assert_eq!(envelope["counts"]["xxx"], 1);

    for marker in envelope["markers"].as_array().expect("markers") {
        assert_eq!(marker["kind"], "DebtMarker");
        assert_eq!(marker["trust"], "source_fact");
        assert!(
            marker["record_id"]
                .as_str()
                .expect("record_id")
                .starts_with("codegraph:v"),
            "each marker must carry a stable record ID"
        );
        let span = &marker["span"];
        assert!(span["start_line"].as_u64().expect("start_line") >= 1);
        assert!(
            span["end_byte"].as_u64().expect("end_byte")
                > span["start_byte"].as_u64().expect("start_byte")
        );
    }

    // The top-level doc-comment marker has no DEFINES/CONTAINS owner: it must
    // carry an explicit serialized null, never a silently omitted field.
    assert!(
        stdout.contains("\"enclosing_symbol\": null"),
        "the top-level marker's null must be serialized explicitly: {stdout}"
    );

    // The string-literal decoy sits on line 4 and the benign comments on
    // lines 6 and 13 of src/lib.rs; no returned span may start there.
    for marker in envelope["markers"].as_array().expect("markers") {
        let start_line = marker["span"]["start_line"].as_u64().expect("line");
        assert!(
            !matches!(start_line, 4 | 6 | 13),
            "decoy line {start_line} must never be returned"
        );
    }
}

#[test]
fn debt_markers_scan_emits_debt_marker_records() {
    let (_temp, graph) = fixture_scanned();
    let jsonl = fs::read_to_string(&graph).expect("graph should read");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let markers: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::DebtMarker,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(markers.len(), 4, "scan must emit one record per marker");
    for record in markers {
        let GraphRecord::Node {
            name,
            language,
            span,
            repo_relative_path,
            note,
            ..
        } = record
        else {
            unreachable!()
        };
        assert!(matches!(
            name.as_deref(),
            Some("todo" | "fixme" | "hack" | "xxx")
        ));
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(span.is_some(), "each marker must carry a source span");
        assert!(repo_relative_path.is_some());
        assert!(note.is_some(), "each marker must carry its note text");
    }
}

// ---------------------------------------------------------------------------
// Scope: prefix filter, empty-vs-not-found honesty, --repo
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_prefix_scopes_results_segment_aware() {
    let (_temp, graph) = fixture_scanned();
    let (envelope, _) = run_lane(&graph, &["--path", "src"]);
    assert_eq!(envelope["counts"]["total"], 4);
    assert_eq!(envelope["path_prefix"], "src");
}

#[test]
fn debt_markers_empty_scope_is_distinguished_from_scope_not_found() {
    let (_temp, graph) = fixture_scanned();

    // Scope exists but contains zero markers: ok envelope with a stable
    // machine-readable empty reason, exit 0.
    let (envelope, _) = run_lane(&graph, &["--path", "src/empty"]);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["markers"], serde_json::json!([]));
    assert_eq!(envelope["counts"]["total"], 0);
    assert_eq!(envelope["empty_reason"], "no_markers_in_scope");

    // Scope not present in the store: scope_not_found, exit 2 — never a
    // silent empty result (issue #196 honesty contract).
    egregore()
        .args(["query", "debt-markers", "--graph"])
        .arg(&graph)
        .args(["--path", "src/nonexistent"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Sibling-prefix bleed: `src/emp` must not match `src/empty`.
    egregore()
        .args(["query", "debt-markers", "--graph"])
        .arg(&graph)
        .args(["--path", "src/emp"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"scope_not_found\""));

    // Malformed prefix: exit 1 with a machine-readable diagnostic.
    egregore()
        .args(["query", "debt-markers", "--graph"])
        .arg(&graph)
        .args(["--path", "/"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"malformed_prefix\""));
}

#[test]
fn debt_markers_repo_selector_scopes_and_rejects_unknown() {
    let (_temp, graph) = fixture_scanned();

    // The override identity resolves as a repo selector.
    let (envelope, _) = run_lane(&graph, &["--repo", "debt-markers-fixture"]);
    assert_eq!(envelope["counts"]["total"], 4);

    // An unknown selector is a documented diagnostic, never a silent empty.
    egregore()
        .args(["query", "debt-markers", "--graph"])
        .arg(&graph)
        .args(["--repo", "no-such-repo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown_repository_selector"));
}

// ---------------------------------------------------------------------------
// Temporal selector (--at, valid-time axis keyed by commit)
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_at_commit_pins_the_inventory_to_valid_time() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let first = commit(&repo, "no debt yet", "2026-01-01T00:00:00Z");

    write(
        &repo,
        "src/lib.rs",
        "pub fn calm() -> u32 {\n    // TODO: remove magic number\n    1\n}\n",
    );
    let second = commit(&repo, "introduce todo", "2026-01-02T00:00:00Z");

    write(&repo, "src/lib.rs", "pub fn calm() -> u32 {\n    1\n}\n");
    let third = commit(&repo, "remove todo", "2026-01-03T00:00:00Z");

    let graph_path = temp.path().join("history.graph.jsonl");
    let jsonl = scan_repository_history_with_override(&repo, Some("debt-markers-history"))
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    fs::write(&graph_path, jsonl).expect("graph should write");

    // Pinned before the introduction: the marker must not appear.
    let (at_first, _) = run_lane(&graph_path, &["--at", &first]);
    assert_eq!(at_first["counts"]["total"], 0);
    assert_eq!(at_first["empty_reason"], "no_markers_in_scope");
    assert_eq!(at_first["at_commit"], Value::String(first));

    // Pinned at the introducing commit: exactly one marker.
    let (at_second, _) = run_lane(&graph_path, &["--at", &second]);
    assert_eq!(at_second["counts"]["total"], 1);
    let tuples = marker_tuples(&at_second);
    assert_eq!(tuples[0].0, "todo");
    assert_eq!(tuples[0].1, "remove magic number");
    assert_eq!(tuples[0].3, Some("calm".to_owned()));
    let marker = &at_second["markers"][0];
    assert_eq!(marker["git_commit"].as_str(), Some(second.as_str()));
    assert!(marker["valid_time"].as_str().is_some());

    // Pinned after the removal: gone again.
    let (at_third, _) = run_lane(&graph_path, &["--at", &third]);
    assert_eq!(at_third["counts"]["total"], 0);

    // A commit the store has never seen is a documented diagnostic (exit 2),
    // never a silent empty result.
    egregore()
        .args(["query", "debt-markers", "--graph"])
        .arg(&graph_path)
        .args(["--at", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"unknown_commit\""));
}

// ---------------------------------------------------------------------------
// Determinism + read-only guarantees
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_output_is_byte_identical_across_five_runs() {
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
fn debt_markers_query_is_read_only() {
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

// ---------------------------------------------------------------------------
// Pre-ingest referential integrity (issue #103)
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_graph_passes_eg_validate() {
    // `File CONTAINS DebtMarker` edges must be referentially closed under the
    // issue #103 gate: the marker fixture graph validates clean (exit 0).
    let (_temp, graph) = fixture_scanned();
    let output = egregore()
        .arg("validate")
        .arg(&graph)
        .output()
        .expect("validate should execute");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    assert_eq!(
        output.status.code(),
        Some(0),
        "debt-marker graph must pass eg validate\nstdout:\n{stdout}"
    );
    let summary: Value = serde_json::from_str(
        stdout
            .lines()
            .last()
            .expect("validate must print a summary line"),
    )
    .expect("summary line should be JSON");
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["defects"], 0);
}

// ---------------------------------------------------------------------------
// Multi-repository stores: enclosing symbols never cross the repo boundary
// ---------------------------------------------------------------------------

#[test]
fn debt_markers_enclosing_symbol_never_cites_another_repository() {
    // Two repositories in one shared store, both defining `src/lib.rs`. The
    // decoy repository's symbol span is deliberately NARROWER than the
    // marker repository's enclosing function while still containing the
    // marker's byte range, so a path-only minimum-width selection would pick
    // the wrong repository's symbol.
    let temp = tempfile::tempdir().expect("temp dir");

    let marker_repo = temp.path().join("marker-repo");
    fs::create_dir_all(&marker_repo).expect("marker repo dir");
    init_git(&marker_repo);
    write(
        &marker_repo,
        "src/lib.rs",
        "pub fn alpha_wide() -> u32 {\n    // TODO: cross repo note\n    let filler_a = 1;\n    let filler_b = 2;\n    let filler_c = 3;\n    let filler_d = 4;\n    filler_a + filler_b + filler_c + filler_d\n}\n",
    );
    commit(&marker_repo, "seed marker repo", "2026-01-01T00:00:00Z");

    let decoy_repo = temp.path().join("decoy-repo");
    fs::create_dir_all(&decoy_repo).expect("decoy repo dir");
    init_git(&decoy_repo);
    write(
        &decoy_repo,
        "src/lib.rs",
        "pub fn narrow_decoy() -> u32 {\n    let pad_one = 11;\n    let pad_two = 22;\n    pad_one + pad_two\n}\n",
    );
    commit(&decoy_repo, "seed decoy repo", "2026-01-01T00:00:00Z");

    let marker_jsonl =
        scan_repository_at_with_override(&marker_repo, "2026-01-01T00:00:00Z", Some("marker-repo"))
            .expect("marker repo should scan")
            .to_jsonl()
            .expect("marker graph should serialize");
    let decoy_jsonl =
        scan_repository_at_with_override(&decoy_repo, "2026-01-01T00:00:00Z", Some("decoy-repo"))
            .expect("decoy repo should scan")
            .to_jsonl()
            .expect("decoy graph should serialize");
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, format!("{marker_jsonl}{decoy_jsonl}")).expect("combined graph writes");

    // Self-validating fixture preconditions: the decoy symbol's span must
    // contain the marker span AND be narrower than the real owner's span,
    // otherwise this test would pass vacuously.
    let records: Vec<Value> = fs::read_to_string(&graph)
        .expect("graph should read")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();
    let span_of = |name: &str| -> (u64, u64) {
        let node = records
            .iter()
            .find(|r| r["record_type"] == "node" && r["name"] == name)
            .unwrap_or_else(|| panic!("fixture must define {name}"));
        (
            node["span"]["start_byte"].as_u64().expect("start"),
            node["span"]["end_byte"].as_u64().expect("end"),
        )
    };
    let marker_node = records
        .iter()
        .find(|r| r["kind"] == "DebtMarker")
        .expect("fixture must contain the marker");
    let marker_span = (
        marker_node["span"]["start_byte"].as_u64().expect("start"),
        marker_node["span"]["end_byte"].as_u64().expect("end"),
    );
    let wide = span_of("alpha_wide");
    let narrow = span_of("narrow_decoy");
    assert!(
        narrow.0 <= marker_span.0 && marker_span.1 <= narrow.1,
        "precondition: decoy span {narrow:?} must contain marker span {marker_span:?}"
    );
    assert!(
        narrow.1 - narrow.0 < wide.1 - wide.0,
        "precondition: decoy span {narrow:?} must be narrower than owner span {wide:?}"
    );

    // Unscoped: the marker's enclosing symbol must come from its own repo.
    let (envelope, _) = run_lane(&graph, &[]);
    assert_eq!(envelope["counts"]["total"], 1);
    assert_eq!(
        envelope["markers"][0]["enclosing_symbol"]["name"], "alpha_wide",
        "the enclosing symbol must never cite another repository's symbol, \
         even when that symbol's span is narrower"
    );
    assert_eq!(envelope["markers"][0]["repository"], "marker-repo");

    // Repo-scoped: same answer — scoping the marker must also scope the
    // candidate enclosing symbols.
    let (scoped, _) = run_lane(&graph, &["--repo", "marker-repo"]);
    assert_eq!(scoped["counts"]["total"], 1);
    assert_eq!(
        scoped["markers"][0]["enclosing_symbol"]["name"], "alpha_wide",
        "a --repo-scoped query must not cite a symbol outside the scope"
    );
}

// ---------------------------------------------------------------------------
// Embedded store round-trip
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn debt_markers_round_trip_through_embedded_store() {
    let (temp, graph) = fixture_scanned();
    let data_dir = temp.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let output = egregore()
        .args(["query", "debt-markers", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("query should execute");
    assert!(
        output.status.success(),
        "embedded query failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be one JSON envelope");
    assert_eq!(envelope["counts"]["total"], 4);
    let notes: Vec<String> = marker_tuples(&envelope)
        .into_iter()
        .map(|(_, note, _, _)| note)
        .collect();
    assert!(
        notes.contains(&"wire retry logic".to_owned()),
        "note text must round-trip through the embedded adapter: {notes:?}"
    );
}
