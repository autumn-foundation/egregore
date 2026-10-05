//! Integration tests for `eg query origin <SYMBOL>` (issue #159).
//!
//! Traces a code symbol to its introducing commit and deterministically
//! links that commit to project-graph PR / issue / review records via exact
//! `merge_commit_sha` equality — no fuzzy title matching, no live GitHub
//! calls. Code facts and project facts stay in trust-separated sections.
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{GraphRecord, NodeKind, SourceSpan, TemporalMetadata};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn temporal_with_parents(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|&s| s.to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    }
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

fn commit_node(id: &str, sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("commit {sha}"),
    )
    .with_temporal(temporal_with_parents(sha, parents, valid_time))
}

fn symbol_node(id: &str, name: &str, sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 10)),
        Some(name.to_owned()),
        format!("fn {name}"),
    )
    .with_temporal(temporal_with_parents(sha, parents, valid_time))
}

/// A GitHub-imported PR `Task` record carrying the promoted flat
/// `merge_commit_sha` field (issue #333 contract). Built through the public
/// builder, then patched at the JSON level — the same flat fields the real
/// importer promotes.
fn pr_task(id: &str, number: u64, title: &str, merge_commit_sha: Option<&str>) -> GraphRecord {
    let mut value = serde_json::to_value(GraphRecord::node(
        id.to_owned(),
        NodeKind::Task,
        None,
        None,
        Some(format!("PR #{number}")),
        format!("task {title}"),
    ))
    .unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.insert(
        "importer_id".to_owned(),
        serde_json::Value::String("github".to_owned()),
    );
    // Project records validate against PROJECT_SCHEMA_VERSION (1), not the
    // code-graph SCHEMA_VERSION the builder stamps by default.
    obj.insert("schema_version".to_owned(), serde_json::json!(1));
    obj.insert(
        "source_kind".to_owned(),
        serde_json::Value::String("github_pr".to_owned()),
    );
    obj.insert(
        "title".to_owned(),
        serde_json::Value::String(title.to_owned()),
    );
    if let Some(sha) = merge_commit_sha {
        obj.insert(
            "merge_commit_sha".to_owned(),
            serde_json::Value::String(sha.to_owned()),
        );
        obj.insert(
            "merged_at".to_owned(),
            serde_json::Value::String("2026-01-03T00:00:00Z".to_owned()),
        );
    }
    serde_json::from_value(value).unwrap()
}

/// A GitHub-imported `Review` record (issue #334 contract).
fn review_node(id: &str, review_kind: &str, merge_commit_sha: Option<&str>) -> GraphRecord {
    let mut value = serde_json::to_value(GraphRecord::node(
        id.to_owned(),
        NodeKind::Review,
        None,
        None,
        Some(format!("{review_kind} on #10")),
        format!("review {review_kind}"),
    ))
    .unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.insert(
        "importer_id".to_owned(),
        serde_json::Value::String("github".to_owned()),
    );
    // Project records validate against PROJECT_SCHEMA_VERSION (1).
    obj.insert("schema_version".to_owned(), serde_json::json!(1));
    obj.insert(
        "source_kind".to_owned(),
        serde_json::Value::String("github_review".to_owned()),
    );
    obj.insert(
        "review_kind".to_owned(),
        serde_json::Value::String(review_kind.to_owned()),
    );
    if let Some(sha) = merge_commit_sha {
        obj.insert(
            "merge_commit_sha".to_owned(),
            serde_json::Value::String(sha.to_owned()),
        );
    }
    serde_json::from_value(value).unwrap()
}

/// History fixture: c1 <- c2 <- c3; symbol `answer` introduced at c2,
/// modified at c3.
fn fixture_history() -> Vec<GraphRecord> {
    vec![
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        commit_node("commit:3", "c3", &["c2"], "2026-01-03T00:00:00Z"),
        symbol_node(
            "symbol:answer",
            "answer",
            "c2",
            &["c1"],
            "2026-01-02T00:00:00Z",
        ),
        symbol_node(
            "symbol:answer",
            "answer",
            "c3",
            &["c2"],
            "2026-01-03T00:00:00Z",
        ),
    ]
}

fn write_graph(records: &[GraphRecord]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.jsonl");
    let content = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, content).unwrap();
    (temp, path)
}

fn run_origin(graph: &PathBuf, args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = egregore();
    cmd.arg("query").arg("origin").args(args).arg("--graph");
    cmd.arg(graph);
    cmd.assert()
}

#[test]
fn origin_happy_path_links_pr_by_exact_sha() {
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![
                pr_task("project:v1:task:10", 10, "Add answer", Some("c2")),
                pr_task("project:v1:task:11", 11, "Unrelated", Some("c9")),
            ],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(value["symbol_name"], "answer");
    // Code facts: introducing commit is c2 (first appearance), not c3.
    assert_eq!(value["code"]["introducing_commit"], "c2");
    assert_eq!(
        value["code"]["introducing_valid_time"],
        "2026-01-02T00:00:00Z"
    );
    assert_eq!(value["code"]["symbol_record_id"], "symbol:answer");
    assert_eq!(value["code"]["trust"], "source_derived");
    // Project facts: only the PR whose merge_commit_sha == "c2" matches.
    let prs = value["project"]["pull_requests"].as_array().unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["record_id"], "project:v1:task:10");
    assert_eq!(prs[0]["merge_commit_sha"], "c2");
    assert_eq!(prs[0]["trust"], "project_state");
    assert_eq!(value["project"]["github_import"], "present");
    // Trust-separated: code section carries no project rows and vice versa.
    assert!(value["code"].get("pull_requests").is_none());
    assert!(value["project"].get("introducing_commit").is_none());
}

#[test]
fn origin_sha_match_is_byte_exact_not_prefix() {
    // A PR whose merge_commit_sha merely shares a prefix with the
    // introducing commit must NOT match (AC2: equality only).
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![pr_task(
                "project:v1:task:10",
                10,
                "Add answer",
                Some("c2-extra"),
            )],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(value["code"]["introducing_commit"], "c2");
    assert!(
        value["project"]["pull_requests"]
            .as_array()
            .unwrap()
            .is_empty(),
        "prefix-similar merge_commit_sha must not match"
    );
}

#[test]
fn origin_no_matching_pr_leaves_section_empty() {
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![pr_task("project:v1:task:11", 11, "Unrelated", Some("c9"))],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(value["project"]["github_import"], "present");
    assert!(
        value["project"]["pull_requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(value["project"]["issues"].as_array().unwrap().is_empty());
    assert!(value["project"]["reviews"].as_array().unwrap().is_empty());
}

#[test]
fn origin_github_import_absent_degrades_with_note() {
    // Code history exists, but no GitHub-imported records: commit-only
    // result with an explicit github_import_absent note — never an error.
    let (_temp, graph) = write_graph(&fixture_history());

    let output = run_origin(&graph, &["answer"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(value["code"]["introducing_commit"], "c2");
    assert_eq!(value["project"]["github_import"], "absent");
    let note = value["project"]["note"].as_str().unwrap();
    assert!(
        note.contains("github_import_absent"),
        "note must carry the github_import_absent marker, got: {note}"
    );
}

#[test]
fn origin_unknown_symbol_is_no_match_exit_2() {
    let (_temp, graph) = write_graph(&fixture_history());

    let output = run_origin(&graph, &["nope"]).code(2);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "no_match");
}

#[test]
fn origin_ambiguous_symbol_reports_candidates_exit_1() {
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![symbol_node(
                "symbol:answer2",
                "answer",
                "c3",
                &["c2"],
                "2026-01-03T00:00:00Z",
            )],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer"]).code(1);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "ambiguous_symbol");
    assert!(!value["error"]["candidates"].as_array().unwrap().is_empty());
}

#[test]
fn origin_symbol_without_history_is_no_history_exit_2() {
    // Snapshot-only store: the symbol exists but has no commit-linked
    // history, so there is no introducing commit to cite.
    let symbol = GraphRecord::node(
        "symbol:answer".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 10)),
        Some("answer".to_owned()),
        "fn answer".to_owned(),
    );
    let (_temp, graph) = write_graph(std::slice::from_ref(&symbol));

    let output = run_origin(&graph, &["answer"]).code(2);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "no_history");
}

#[test]
fn origin_at_pin_before_introduction_is_no_match() {
    let (_temp, graph) = write_graph(&fixture_history());

    // --at c1: symbol `answer` did not exist yet -> no_match, exit 2.
    let output = run_origin(&graph, &["answer", "--at", "c1"]).code(2);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["code"], "no_match");

    // --at c2 (unique prefix "c2" resolves to the full commit): origin is c2.
    let output = run_origin(&graph, &["answer", "--at", "c2"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["code"]["introducing_commit"], "c2");
    assert_eq!(value["code"]["temporal_selector"]["at"], "c2");
}

#[test]
fn origin_as_of_before_introduction_is_no_match() {
    let (_temp, graph) = write_graph(&fixture_history());

    // Before c2's valid time the symbol did not exist.
    let output = run_origin(&graph, &["answer", "--as-of", "2026-01-01T12:00:00Z"]).code(2);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["code"], "no_match");

    // After introduction, the origin is still the first introduction (c2).
    let output = run_origin(&graph, &["answer", "--as-of", "2026-01-03T12:00:00Z"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["code"]["introducing_commit"], "c2");
}

#[test]
fn origin_at_unknown_commit_is_missing_commit_exit_2() {
    let (_temp, graph) = write_graph(&fixture_history());

    let output = run_origin(&graph, &["answer", "--at", "deadbeef"]).code(2);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["code"], "missing_commit");
}

#[test]
fn origin_malformed_as_of_is_exit_1() {
    let (_temp, graph) = write_graph(&fixture_history());

    let output = run_origin(&graph, &["answer", "--as-of", "not-a-time"]).code(1);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["error"]["code"], "invalid_as_of");
}

#[test]
fn origin_classifies_matched_records_by_kind() {
    // A Review node carrying merge_commit_sha == introducing commit lands in
    // `reviews`, not `pull_requests` (generic field-equality rule).
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![
                pr_task("project:v1:task:10", 10, "Add answer", Some("c2")),
                review_node("project:v1:review:1", "pr_review", Some("c2")),
            ],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(
        value["project"]["pull_requests"].as_array().unwrap().len(),
        1
    );
    let reviews = value["project"]["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["record_id"], "project:v1:review:1");
}

#[test]
fn origin_text_format_prints_summary() {
    let (_temp, graph) = write_graph(
        &[
            fixture_history(),
            vec![pr_task("project:v1:task:10", 10, "Add answer", Some("c2"))],
        ]
        .concat(),
    );

    let output = run_origin(&graph, &["answer", "--format", "text"]).success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("c2"),
        "text summary should name the introducing commit, got: {stdout}"
    );
    assert!(
        stdout.contains("PR #10") || stdout.contains("project:v1:task:10"),
        "text summary should name the matched PR, got: {stdout}"
    );
}
