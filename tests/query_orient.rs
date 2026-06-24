//! Integration tests for `eg query orient` (issue #95).
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{EdgeLabel, GraphRecord, NodeKind};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

#[allow(clippy::too_many_lines)]
fn fixture_happy_path() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("graph.jsonl");

    let repo_node = GraphRecord::node(
        "repo:test".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("test-repo".to_owned()),
        "Test repository".to_owned(),
    );

    let lib_file = GraphRecord::node(
        "file:lib".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "file src/lib.rs".to_owned(),
    );
    let bin_file = GraphRecord::node(
        "file:bin".to_owned(),
        NodeKind::File,
        Some("src/bin/tool.rs".to_owned()),
        None,
        Some("src/bin/tool.rs".to_owned()),
        "file src/bin/tool.rs".to_owned(),
    );
    let helper_file = GraphRecord::node(
        "file:helper".to_owned(),
        NodeKind::File,
        Some("src/helper.rs".to_owned()),
        None,
        Some("src/helper.rs".to_owned()),
        "file src/helper.rs".to_owned(),
    );

    let lib_fn = GraphRecord::symbol(
        "symbol:lib_fn".to_owned(),
        "fn",
        "src/lib.rs".to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 2,
        },
        "lib_fn".to_owned(),
        "fn lib_fn".to_owned(),
    );
    let bin_fn = GraphRecord::symbol(
        "symbol:bin_fn".to_owned(),
        "fn",
        "src/bin/tool.rs".to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 2,
        },
        "bin_fn".to_owned(),
        "fn bin_fn".to_owned(),
    );
    let helper_fn = GraphRecord::symbol(
        "symbol:helper_fn".to_owned(),
        "fn",
        "src/helper.rs".to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 5,
            end_line: 6,
        },
        "helper_fn".to_owned(),
        "fn helper_fn".to_owned(),
    );

    let e_repo_lib = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo:test".to_owned(),
        "file:lib".to_owned(),
        None,
        "repo contains lib".to_owned(),
    );
    let e_repo_bin = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo:test".to_owned(),
        "file:bin".to_owned(),
        None,
        "repo contains bin".to_owned(),
    );
    let e_repo_helper = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo:test".to_owned(),
        "file:helper".to_owned(),
        None,
        "repo contains helper".to_owned(),
    );

    let e_lib_fn = GraphRecord::edge(
        EdgeLabel::Defines,
        "file:lib".to_owned(),
        "symbol:lib_fn".to_owned(),
        None,
        "lib defines lib_fn".to_owned(),
    );
    let e_bin_fn = GraphRecord::edge(
        EdgeLabel::Defines,
        "file:bin".to_owned(),
        "symbol:bin_fn".to_owned(),
        None,
        "bin defines bin_fn".to_owned(),
    );
    let e_helper_fn = GraphRecord::edge(
        EdgeLabel::Defines,
        "file:helper".to_owned(),
        "symbol:helper_fn".to_owned(),
        None,
        "helper defines helper_fn".to_owned(),
    );

    let e_call1 = GraphRecord::edge(
        EdgeLabel::Calls,
        "symbol:lib_fn".to_owned(),
        "symbol:helper_fn".to_owned(),
        None,
        "lib_fn calls helper_fn".to_owned(),
    );
    let e_call2 = GraphRecord::edge(
        EdgeLabel::Calls,
        "symbol:bin_fn".to_owned(),
        "symbol:helper_fn".to_owned(),
        None,
        "bin_fn calls helper_fn".to_owned(),
    );

    let records = vec![
        repo_node,
        lib_file,
        bin_file,
        helper_file,
        lib_fn,
        bin_fn,
        helper_fn,
        e_repo_lib,
        e_repo_bin,
        e_repo_helper,
        e_lib_fn,
        e_bin_fn,
        e_helper_fn,
        e_call1,
        e_call2,
    ];

    let content = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&graph_path, content).unwrap();

    (temp, graph_path)
}

#[test]
fn query_orient_happy_path_json() {
    let (_temp, graph) = fixture_happy_path();

    let output = egregore()
        .args(["query", "orient", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];

    // Entry points
    let entry_points = result["entry_points"]
        .as_array()
        .expect("entry points array");
    assert_eq!(entry_points.len(), 2);
    assert_eq!(
        entry_points[0]["repo_relative_path"].as_str(),
        Some("src/bin/tool.rs")
    );
    assert_eq!(
        entry_points[1]["repo_relative_path"].as_str(),
        Some("src/lib.rs")
    );

    // Module tree
    let module_tree = result["module_tree"].as_array().expect("module tree array");
    assert_eq!(module_tree.len(), 1);
    assert_eq!(module_tree[0]["name"].as_str(), Some("src"));
    assert_eq!(module_tree[0]["symbol_count"].as_u64(), Some(3));

    // Top referenced symbols
    let top_symbols = result["top_referenced_symbols"]
        .as_array()
        .expect("top referenced symbols");
    assert_eq!(top_symbols.len(), 3);
    assert_eq!(top_symbols[0]["name"].as_str(), Some("helper_fn"));
    assert_eq!(top_symbols[0]["inbound_degree"].as_u64(), Some(3)); // Defines (1) + Calls (2) = 3
}

#[test]
fn query_orient_happy_path_text() {
    let (_temp, graph) = fixture_happy_path();

    let output = egregore()
        .args(["query", "orient", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("Entry Points:"));
    assert!(stdout.contains("- src/lib.rs (file:lib)"));
    assert!(stdout.contains("Module/File Tree:"));
    assert!(stdout.contains("- src/ [no_span_module_level] (3 symbols)"));
    assert!(stdout.contains("  - bin/ [no_span_module_level] (1 symbols)"));
    assert!(stdout.contains("    - tool.rs (file:bin) @ src/bin/tool.rs (1 symbols)"));
    assert!(stdout.contains("Top Referenced Symbols:"));
    assert!(stdout.contains("1. helper_fn degree=3 @ src/helper.rs:5 (symbol:helper_fn)"));
}

#[test]
fn query_orient_empty_graph_exits_3() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("empty.jsonl");
    fs::write(&graph, "").unwrap();

    let output = egregore()
        .args(["query", "orient", "--graph"])
        .arg(&graph)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"].as_str(), Some("empty_graph"));
}

#[test]
fn query_orient_no_entry_points_exits_4() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("no_ep.jsonl");

    let helper_file = GraphRecord::node(
        "file:helper".to_owned(),
        NodeKind::File,
        Some("src/helper.rs".to_owned()),
        None,
        Some("src/helper.rs".to_owned()),
        "file src/helper.rs".to_owned(),
    );

    let content = serde_json::to_string(&helper_file).unwrap() + "\n";
    fs::write(&graph, content).unwrap();

    let output = egregore()
        .args(["query", "orient", "--graph"])
        .arg(&graph)
        .assert()
        .code(4)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"].as_str(), Some("no_entry_points"));
}

#[test]
fn query_orient_determinism_five_runs() {
    let (_temp, graph) = fixture_happy_path();

    let get_output = || {
        let output = egregore()
            .args(["query", "orient", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).unwrap()
    };

    let first = get_output();
    for _ in 0..5 {
        assert_eq!(get_output(), first);
    }
}

#[test]
fn query_orient_scoped_repo() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("scoped.jsonl");

    let repo_a = GraphRecord::node(
        "repo:a".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo_a".to_owned()),
        "Repo A".to_owned(),
    );
    let repo_b = GraphRecord::node(
        "repo:b".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo_b".to_owned()),
        "Repo B".to_owned(),
    );

    let file_a = GraphRecord::node(
        "file:a".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "file src/lib.rs".to_owned(),
    );
    let file_b = GraphRecord::node(
        "file:b".to_owned(),
        NodeKind::File,
        Some("src/main.rs".to_owned()),
        None,
        Some("src/main.rs".to_owned()),
        "file src/main.rs".to_owned(),
    );

    let e_contains_a = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo:a".to_owned(),
        "file:a".to_owned(),
        None,
        "repo contains file_a".to_owned(),
    );
    let e_contains_b = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo:b".to_owned(),
        "file:b".to_owned(),
        None,
        "repo contains file_b".to_owned(),
    );

    let records = vec![repo_a, repo_b, file_a, file_b, e_contains_a, e_contains_b];
    let content = records
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&graph, content).unwrap();

    // Query scoped to repo_a
    let output = egregore()
        .args(["query", "orient", "--graph"])
        .arg(&graph)
        .args(["--repo", "repo_a"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], true);

    // Should only return entry points for repo_a (src/lib.rs)
    let entry_points = parsed["result"]["entry_points"].as_array().unwrap();
    assert_eq!(entry_points.len(), 1);
    assert_eq!(
        entry_points[0]["repo_relative_path"].as_str(),
        Some("src/lib.rs")
    );
}
