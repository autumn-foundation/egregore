#![allow(missing_docs)]
//! Phantom same-file `CALLS`/`REFERENCES` edge suppression (issue #134).
//!
//! The README claims typed edges carry "no false positives from comments or
//! string literals". These tests back that sentence: a symbol name that
//! appears only inside a comment, only inside a string literal, or only as a
//! substring of a longer identifier must never produce a `CALLS`,
//! `REFERENCES`, or `MENTIONS` edge to that symbol. Same-file `CALLS` edges
//! that correspond to a Tree-sitter call site additionally carry the shared
//! `resolution` status (`resolved` / `ambiguous`) coordinated with issue #152.

use std::{fs, path::Path};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "phantom-call-edges-fixture";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn scan_jsonl(root: &Path) -> String {
    scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    parse_jsonl(&scan_jsonl(root))
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol_id(records: &[Value], symbol_kind: &str, name: &str, path: &str) -> String {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["name"] == name
                && record["repo_relative_path"] == path
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol {name} in {path}"))["id"]
        .as_str()
        .expect("symbol should have an ID")
        .to_owned()
}

/// All CALLS / REFERENCES / MENTIONS edges pointing at `target`.
fn reference_like_edges_to<'a>(records: &'a [Value], target: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["target"] == target
                && matches!(
                    record["label"].as_str(),
                    Some("CALLS" | "REFERENCES" | "MENTIONS")
                )
        })
        .collect()
}

fn edge<'a>(records: &'a [Value], label: &str, source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == label
            && record["source"] == source
            && record["target"] == target
    })
}

// ---------------------------------------------------------------------------
// AC1: comment-only mentions produce zero edges (backs the README claim)
// ---------------------------------------------------------------------------

#[test]
fn comment_only_mention_produces_no_same_file_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn quiet_target() -> usize {\n    3\n}\n\npub fn documented() -> usize {\n    // quiet_target() would also work here\n    /* quiet_target is discussed in this block comment */\n    5\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let quiet_target = symbol_id(&records, "function", "quiet_target", "src/lib.rs");

    let offending = reference_like_edges_to(&records, &quiet_target);
    assert!(
        offending.is_empty(),
        "a name that appears only inside comments must not produce edges: {offending:?}"
    );
}

// ---------------------------------------------------------------------------
// AC2: string-literal-only mentions produce zero edges
// ---------------------------------------------------------------------------

#[test]
fn string_literal_only_mention_produces_no_same_file_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn quiet_target() -> usize {\n    3\n}\n\npub fn stringy() -> &'static str {\n    let _raw = r#\"quiet_target() inside a raw string\"#;\n    \"call quiet_target() later\"\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let quiet_target = symbol_id(&records, "function", "quiet_target", "src/lib.rs");

    let offending = reference_like_edges_to(&records, &quiet_target);
    assert!(
        offending.is_empty(),
        "a name that appears only inside string literals must not produce edges: {offending:?}"
    );
}

// ---------------------------------------------------------------------------
// AC3: substring occurrences never classify as calls
// ---------------------------------------------------------------------------

#[test]
fn substring_occurrence_is_a_reference_not_a_call() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    // `dispatcher` mentions `run` only as a standalone identifier (`let chosen
    // = run;`) and calls `prerun()`. The `run(` substring inside `prerun(`
    // must not upgrade the standalone mention into a CALLS edge.
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn run() -> usize {\n    1\n}\n\npub fn prerun() -> usize {\n    2\n}\n\npub fn dispatcher() -> usize {\n    let chosen = run;\n    prerun() + chosen()\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let run = symbol_id(&records, "function", "run", "src/lib.rs");
    let prerun = symbol_id(&records, "function", "prerun", "src/lib.rs");
    let dispatcher = symbol_id(&records, "function", "dispatcher", "src/lib.rs");

    assert!(
        edge(&records, "CALLS", &dispatcher, &run).is_none(),
        "`run(` inside `prerun(` must not classify the standalone `run` mention as a call"
    );
    assert!(
        edge(&records, "REFERENCES", &dispatcher, &run).is_some(),
        "the standalone `run` identifier mention should stay a REFERENCES edge"
    );
    assert!(
        edge(&records, "CALLS", &dispatcher, &prerun).is_some(),
        "the real prerun() call site must keep its CALLS edge"
    );
}

#[test]
fn substring_only_occurrence_produces_no_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn run() -> usize {\n    1\n}\n\npub fn prerun() -> usize {\n    2\n}\n\npub fn caller() -> usize {\n    prerun()\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let run = symbol_id(&records, "function", "run", "src/lib.rs");
    let caller = symbol_id(&records, "function", "caller", "src/lib.rs");

    let offending: Vec<&Value> = reference_like_edges_to(&records, &run)
        .into_iter()
        .filter(|record| record["source"] == caller.as_str())
        .collect();
    assert!(
        offending.is_empty(),
        "`run` occurring only inside `prerun` must not produce edges to `run`: {offending:?}"
    );
}

// ---------------------------------------------------------------------------
// AC4: same-file CALLS edges carry the shared resolution status
// ---------------------------------------------------------------------------

#[test]
fn same_file_call_to_unique_definition_is_labeled_resolved() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn helper() -> usize {\n    1\n}\n\npub fn caller() -> usize {\n    helper()\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "helper", "src/lib.rs");
    let caller = symbol_id(&records, "function", "caller", "src/lib.rs");

    let call_edge = edge(&records, "CALLS", &caller, &helper)
        .expect("same-file call should produce a CALLS edge");
    assert_eq!(
        call_edge["resolution"], "resolved",
        "a same-file call with exactly one in-repo candidate must be labeled resolved: {call_edge}"
    );
}

#[test]
fn same_file_call_with_in_repo_collision_is_labeled_ambiguous() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn dupe() -> usize {\n    1\n}\n\npub fn alpha_caller() -> usize {\n    dupe()\n}\n",
            ),
            ("src/delta.rs", "pub fn dupe() -> usize {\n    2\n}\n"),
        ],
    );

    let records = scan_fixture(repo);
    let alpha_dupe = symbol_id(&records, "function", "alpha::dupe", "src/alpha.rs");
    let alpha_caller = symbol_id(&records, "function", "alpha::alpha_caller", "src/alpha.rs");

    let call_edge = edge(&records, "CALLS", &alpha_caller, &alpha_dupe)
        .expect("same-file call should produce a CALLS edge");
    assert_eq!(
        call_edge["resolution"], "ambiguous",
        "a same-file call whose simple name matches two in-repo definitions must be \
         labeled ambiguous, not asserted as uniquely resolved: {call_edge}"
    );
    assert!(
        call_edge.get("confidence").is_none(),
        "an ambiguous edge must not assert full confidence: {call_edge}"
    );
}

// ---------------------------------------------------------------------------
// AC5: query callers/dependencies output exposes the resolution status
// ---------------------------------------------------------------------------

#[test]
fn change_impact_rows_expose_call_resolution_status() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub fn helper() -> usize {\n    1\n}\n\npub fn caller() -> usize {\n    helper()\n}\n",
        )],
    );

    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(repo)).expect("graph should be written");

    let output = egregore()
        .args([
            "query",
            "change-impact",
            "caller",
            "--graph",
            &graph_path.to_string_lossy(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value =
        serde_json::from_slice(&output).expect("change-impact output should be JSON");

    let callees = response["direct_callees"]
        .as_array()
        .expect("direct_callees should be an array");
    let helper_row = callees
        .iter()
        .find(|row| row["name"] == "helper")
        .expect("helper should appear as a direct callee");
    assert_eq!(
        helper_row["resolution"], "resolved",
        "change-impact rows must expose the CALLS resolution status so agents can \
         filter to resolved-only edges: {helper_row}"
    );
}

// ---------------------------------------------------------------------------
// AC7: output stays deterministic and byte-stable
// ---------------------------------------------------------------------------

#[test]
fn suppression_and_labeling_are_byte_stable_across_scans() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn dupe() -> usize {\n    1\n}\n\npub fn alpha_caller() -> usize {\n    // dupe() discussed in a comment\n    let _s = \"dupe() in a string\";\n    dupe()\n}\n",
            ),
            ("src/delta.rs", "pub fn dupe() -> usize {\n    2\n}\n"),
        ],
    );

    let first = scan_jsonl(repo);
    for _ in 0..3 {
        assert_eq!(
            first,
            scan_jsonl(repo),
            "repeated scans of an unchanged tree must be byte-identical"
        );
    }
}
