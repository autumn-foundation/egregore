#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start: usize, end: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line: start,
        end_line: end,
    }
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_who() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let repo_id = stable_id(&["node", "Repository", "my_repo"]);
    let repo_node = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("my_repo".to_owned()),
        "Repository my_repo".to_owned(),
    );

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let file_node = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );

    // Symbol node at Commit 1
    let sym_id_a = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_node_a = GraphRecord::symbol(
        sym_id_a.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Symbol node at Commit 2
    let sym_id_b = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_node_b = GraphRecord::symbol(
        sym_id_b.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        sym_id_a,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let defines_edge_b = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id,
        sym_id_b,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let contains_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_node.id().to_owned(),
        None,
        "contains file".to_owned(),
    );

    // Commit 1: Alice at 2026-01-01
    let commit1_id = stable_id(&["node", "Commit", "my_repo", "commit1_sha"]);
    let commit1 = GraphRecord::node(
        commit1_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit1_sha".to_owned()),
        "Commit 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(
        Some("Alice".to_owned()),
        Some("alice@example.com".to_owned()),
    );

    // Change 1: in commit 1, modifying src/lib.rs
    let change1_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit1_sha",
        "M",
        "src/lib.rs",
    ]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "change 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Commit 2: Bob at 2026-01-02
    let commit2_id = stable_id(&["node", "Commit", "my_repo", "commit2_sha"]);
    let commit2 = GraphRecord::node(
        commit2_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit2_sha".to_owned()),
        "Commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(Some("Bob".to_owned()), Some("bob@example.com".to_owned()));

    // Change 2: in commit 2, modifying src/lib.rs
    let change2_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit2_sha",
        "M",
        "src/lib.rs",
    ]);
    let change2 = GraphRecord::node(
        change2_id,
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "change 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let contains_commit1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        commit1.id().to_owned(),
        None,
        "contains commit 1".to_owned(),
    );
    let contains_commit2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id,
        commit2.id().to_owned(),
        None,
        "contains commit 2".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(repo_node);
    graph.push(file_node);
    graph.push(sym_node_a);
    graph.push(sym_node_b);
    graph.push(defines_edge_a);
    graph.push(defines_edge_b);
    graph.push(contains_edge);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(commit1);
    graph.push(change1);
    graph.push(commit2);
    graph.push(change2);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn test_query_who_default_latest() {
    let (_temp, graph_path) = fixture_graph_for_who();

    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("scan_repository last changed by Bob <bob@example.com> in commit commit2_sha @ 2026-01-02T00:00:00Z (src/lib.rs)"));
}

#[test]
fn test_query_who_at_selector() {
    let (_temp, graph_path) = fixture_graph_for_who();

    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--at")
        .arg("commit1_sha")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("scan_repository last changed by Alice <alice@example.com> in commit commit1_sha @ 2026-01-01T00:00:00Z (src/lib.rs)"));
}

#[test]
fn test_query_who_as_of_selector() {
    let (_temp, graph_path) = fixture_graph_for_who();

    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--as-of")
        .arg("2026-01-01T12:00:00Z")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("scan_repository last changed by Alice <alice@example.com> in commit commit1_sha @ 2026-01-01T00:00:00Z (src/lib.rs)"));
}

#[test]
fn test_query_who_json_format() {
    let (_temp, graph_path) = fixture_graph_for_who();

    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid JSON");

    assert_eq!(parsed["symbol_name"], "scan_repository");
    assert_eq!(parsed["commit_sha"], "commit2_sha");
    assert_eq!(parsed["author_name"], "Bob");
    assert_eq!(parsed["author_email"], "bob@example.com");
    assert_eq!(parsed["valid_time"], "2026-01-02T00:00:00Z");
    assert_eq!(parsed["repo_relative_path"], "src/lib.rs");
}

#[test]
fn test_query_who_nonexistent_symbol() {
    let (_temp, graph_path) = fixture_graph_for_who();

    egregore()
        .args(["query", "who", "nonexistent_fn", "--graph"])
        .arg(&graph_path)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "error: no match found for symbol `nonexistent_fn`",
        ));
}

#[test]
fn test_query_who_invalid_timestamp() {
    let (_temp, graph_path) = fixture_graph_for_who();

    egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--as-of")
        .arg("not-a-timestamp")
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains(
            "error: invalid --as-of timestamp",
        ));
}

#[test]
fn test_query_who_ambiguous_symbol() {
    let (_temp, graph_path) = fixture_graph_for_ambiguity();

    // 1. Querying without --repo should fail due to ambiguity
    egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains(
            "is defined in multiple repositories",
        ));

    // 2. Querying with --repo repo1 should succeed and return Alice
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("repo1")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains(
        "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
    ));

    // 3. Querying with --repo repo2 should succeed and return Bob
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("repo2")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains(
            "scan_repository last changed by Bob <bob@example.com> in commit commit2_sha"
        )
    );
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_ambiguity() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    // Repo 1
    let repo_id1 = stable_id(&["node", "repository", "repo1"]);
    let repo_node1 = GraphRecord::node(
        repo_id1.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo1".to_owned()),
        "Repository repo1".to_owned(),
    );

    let file_id1 = stable_id(&["node", "file", &repo_id1, "src/main.rs"]);
    let file_node1 = GraphRecord::syntax_node(
        file_id1.clone(),
        NodeKind::File,
        "src/main.rs".to_owned(),
        span(1, 50),
        "main.rs".to_owned(),
        "rust",
        "Source file src/main.rs".to_owned(),
    );

    let contains_edge1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id1.clone(),
        file_id1.clone(),
        None,
        "contains file".to_owned(),
    );

    let sym_id1 = stable_id(&[
        "node",
        "symbol",
        "fn",
        &repo_id1,
        "src/main.rs",
        "scan_repository",
        "0",
    ]);
    let sym_node1 = GraphRecord::symbol(
        sym_id1.clone(),
        "fn",
        "src/main.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id1,
        sym_id1,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let commit1_id = stable_id(&["node", "commit", &repo_id1, "commit1_sha"]);
    let commit1 = GraphRecord::node(
        commit1_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit1_sha".to_owned()),
        "Commit 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(
        Some("Alice".to_owned()),
        Some("alice@example.com".to_owned()),
    );

    let change1_id = stable_id(&[
        "node",
        "change",
        &repo_id1,
        "commit1_sha",
        "M",
        "src/main.rs",
    ]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/main.rs".to_owned()),
        None,
        None,
        "change 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Repo 2
    let repo_id2 = stable_id(&["node", "repository", "repo2"]);
    let repo_node2 = GraphRecord::node(
        repo_id2.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo2".to_owned()),
        "Repository repo2".to_owned(),
    );

    let file_id2 = stable_id(&["node", "file", &repo_id2, "src/main.rs"]);
    let file_node2 = GraphRecord::syntax_node(
        file_id2.clone(),
        NodeKind::File,
        "src/main.rs".to_owned(),
        span(1, 50),
        "main.rs".to_owned(),
        "rust",
        "Source file src/main.rs".to_owned(),
    );

    let contains_edge2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id2.clone(),
        file_id2.clone(),
        None,
        "contains file".to_owned(),
    );

    let sym_id2 = stable_id(&[
        "node",
        "symbol",
        "fn",
        &repo_id2,
        "src/main.rs",
        "scan_repository",
        "0",
    ]);
    let sym_node2 = GraphRecord::symbol(
        sym_id2.clone(),
        "fn",
        "src/main.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id2,
        sym_id2,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let commit2_id = stable_id(&["node", "commit", &repo_id2, "commit2_sha"]);
    let commit2 = GraphRecord::node(
        commit2_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit2_sha".to_owned()),
        "Commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(Some("Bob".to_owned()), Some("bob@example.com".to_owned()));

    let change2_id = stable_id(&[
        "node",
        "change",
        &repo_id2,
        "commit2_sha",
        "M",
        "src/main.rs",
    ]);
    let change2 = GraphRecord::node(
        change2_id,
        NodeKind::Change,
        Some("src/main.rs".to_owned()),
        None,
        None,
        "change 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let contains_commit1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id1,
        commit1.id().to_owned(),
        None,
        "contains commit 1".to_owned(),
    );
    let contains_commit2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id2,
        commit2.id().to_owned(),
        None,
        "contains commit 2".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(repo_node1);
    graph.push(file_node1);
    graph.push(contains_edge1);
    graph.push(contains_commit1);
    graph.push(sym_node1);
    graph.push(defines_edge1);
    graph.push(commit1);
    graph.push(change1);

    graph.push(repo_node2);
    graph.push(file_node2);
    graph.push(contains_edge2);
    graph.push(contains_commit2);
    graph.push(sym_node2);
    graph.push(defines_edge2);
    graph.push(commit2);
    graph.push(change2);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}
