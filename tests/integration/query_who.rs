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
    // Issue #427: a temporal pin (--at/--as-of) discloses the commit-pinned corpus.
    assert!(
        text.contains("corpus: commit_pinned"),
        "text output must disclose the commit-pinned corpus, got: {text}"
    );
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
    // Issue #427: a temporal pin (--at/--as-of) discloses the commit-pinned corpus.
    assert!(
        text.contains("corpus: commit_pinned"),
        "text output must disclose the commit-pinned corpus, got: {text}"
    );
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
    // Issue #427: this fixture carries no `source_snapshot`, so the default
    // current-state view discloses single_snapshot.
    assert_eq!(parsed["corpus_mode"], "single_snapshot");
    assert_eq!(parsed["corpus_mode_source"], "default");
    assert!(parsed["corpus_disclaimer"].as_str().is_some());
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

#[test]
fn test_query_who_ambiguous_at_prefix_resolved_by_repo() {
    let (_temp, graph_path) = fixture_graph_for_ambiguity();

    // 1. Querying with --at commit without --repo should fail as commit prefix is ambiguous
    egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--at")
        .arg("commit")
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains(
            "commit prefix 'commit' is ambiguous",
        ));

    // 2. Querying with --at commit and --repo repo1 should succeed and resolve to commit1_sha
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--at")
        .arg("commit")
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

#[test]
fn test_query_who_duplicate_commit_sha_scoped() {
    let (_temp, graph_path) = fixture_graph_for_duplicate_commits();

    // Querying Repo 1 with --at shared_commit_sha should return Alice
    let output1 = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("repo1")
        .arg("--at")
        .arg("shared")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text1 = String::from_utf8(output1).expect("utf8");
    assert!(text1.contains("Alice <alice@example.com>"));

    // Querying Repo 2 with --at shared_commit_sha should return Bob
    let output2 = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("repo2")
        .arg("--at")
        .arg("shared")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text2 = String::from_utf8(output2).expect("utf8");
    assert!(text2.contains("Bob <bob@example.com>"));
}

#[test]
fn test_query_who_repo_path_freshness() {
    // Set up a Git repository so that we can compute freshness.
    let repo_dir = tempfile::tempdir().unwrap();
    let repo_path = repo_dir.path();

    // Initialize git repository
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("init")
        .output()
        .unwrap();
    assert!(out.status.success());

    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "user.email", "test@example.com"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "commit.gpgsign", "false"])
        .output()
        .unwrap();

    // Create a file and commit it
    let file_path = repo_path.join("src").join("lib.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "pub fn hello() {}\n").unwrap();

    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("add")
        .arg(".")
        .output()
        .unwrap();

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["commit", "-m", "initial"])
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(out.status.success());

    // Generate graph for this repository
    let graph_path = repo_path.join("graph.jsonl");
    egregore()
        .arg("scan-history")
        .arg(repo_path)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Now query "hello" with --repo-path and check for freshness
    let output = egregore()
        .args(["query", "who", "hello", "--graph"])
        .arg(&graph_path)
        .arg("--repo-path")
        .arg(repo_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid JSON");

    // The freshness field should be present (typically "fresh" since the working tree has no new edits)
    assert!(parsed.get("freshness").is_some());
    assert_eq!(parsed["freshness"], "fresh");

    // Let's also check text formatting prints the freshness suffix
    let output_text = egregore()
        .args(["query", "who", "hello", "--graph"])
        .arg(&graph_path)
        .arg("--repo-path")
        .arg(repo_path)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output_text).expect("utf8");
    assert!(text.contains("(freshness: fresh)"));
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_duplicate_commits() -> (tempfile::TempDir, PathBuf) {
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
        git_commit: "shared_commit_sha".to_owned(),
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

    let commit1_id = stable_id(&["node", "commit", &repo_id1, "shared_commit_sha"]);
    let commit1 = GraphRecord::node(
        commit1_id,
        NodeKind::Commit,
        None,
        None,
        Some("shared_commit_sha".to_owned()),
        "Commit 1".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "shared_commit_sha".to_owned(),
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
        "shared_commit_sha",
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
        git_commit: "shared_commit_sha".to_owned(),
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
        git_commit: "shared_commit_sha".to_owned(),
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

    let commit2_id = stable_id(&["node", "commit", &repo_id2, "shared_commit_sha"]);
    let commit2 = GraphRecord::node(
        commit2_id,
        NodeKind::Commit,
        None,
        None,
        Some("shared_commit_sha".to_owned()),
        "Commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "shared_commit_sha".to_owned(),
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
        "shared_commit_sha",
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
        git_commit: "shared_commit_sha".to_owned(),
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

#[test]
fn test_query_who_deleted_symbol_not_found() {
    let (_temp, graph_path) = fixture_graph_for_deleted_symbol();

    // 1. Querying at HEAD (current HEAD is commit2_sha where the symbol is deleted) should fail (exit 2)
    egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "error: no match found for symbol `scan_repository`",
        ));

    // 2. Querying --as-of 2026-01-01 (before deletion) should succeed and return Alice
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
    assert!(text.contains(
        "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
    ));
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_deleted_symbol() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let repo_id = stable_id(&["node", "Repository", "my_repo"]);

    // Commit 2 is HEAD
    let repo_node = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("my_repo".to_owned()),
        "Repository my_repo".to_owned(),
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit2_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-02T12:00:00Z".to_owned(),
    });

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

    // Symbol node is only defined at Commit 1
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

    let defines_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id,
        sym_id_a,
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

    // Commit 2: Bob at 2026-01-02 (deletes the symbol, but modifies file)
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
    graph.push(defines_edge_a);
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
fn test_query_who_only_attributes_actual_symbol_changes() {
    let (_temp, graph_path) = fixture_graph_for_symbol_unchanged_in_later_commit();

    // Querying HEAD (Commit 2) should return Commit 1 (Alice), since Commit 2 did not change the symbol
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
    assert!(
        text.contains(
            "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
        ),
        "Expected last change by Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_symbol_unchanged_in_later_commit() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let repo_id = stable_id(&["node", "Repository", "my_repo"]);

    // Commit 2 is HEAD
    let repo_node = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("my_repo".to_owned()),
        "Repository my_repo".to_owned(),
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit2_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-02T12:00:00Z".to_owned(),
    });

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
        "Rust function scan_repository".to_owned(), // Identical summary to sym_node_b!
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Symbol node at Commit 2 (Bob re-emits same symbol)
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
        "Rust function scan_repository".to_owned(), // Identical summary!
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
        "file defines symbol b".to_owned(),
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
fn test_query_who_attributes_across_renames() {
    let (_temp, graph_path) = fixture_graph_for_renamed_symbol();

    // Querying HEAD (Commit 2, where file path is new_lib.rs) should follow rename back to Commit 1 (Alice)
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
    assert!(
        text.contains(
            "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
        ),
        "Expected last change by Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_for_renamed_symbol() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let repo_id = stable_id(&["node", "Repository", "my_repo"]);

    // Commit 2 is HEAD
    let repo_node = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("my_repo".to_owned()),
        "Repository my_repo".to_owned(),
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit2_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-02T12:00:00Z".to_owned(),
    });

    let old_file_id = stable_id(&["node", "File", "src/old_lib.rs"]);
    let new_file_id = stable_id(&["node", "File", "src/new_lib.rs"]);

    let old_file_node = GraphRecord::syntax_node(
        old_file_id.clone(),
        NodeKind::File,
        "src/old_lib.rs".to_owned(),
        span(1, 50),
        "old_lib.rs".to_owned(),
        "rust",
        "Source file src/old_lib.rs".to_owned(),
    );

    let new_file_node = GraphRecord::syntax_node(
        new_file_id.clone(),
        NodeKind::File,
        "src/new_lib.rs".to_owned(),
        span(1, 50),
        "new_lib.rs".to_owned(),
        "rust",
        "Source file src/new_lib.rs".to_owned(),
    );

    // Symbol node at Commit 1 (in src/old_lib.rs)
    let sym_id_a = stable_id(&[
        "node",
        "Symbol",
        "src/old_lib.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_node_a = GraphRecord::symbol(
        sym_id_a.clone(),
        "fn",
        "src/old_lib.rs".to_owned(),
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

    // Symbol node at Commit 2 (Bob re-emits same symbol under new path src/new_lib.rs)
    let sym_id_b = stable_id(&[
        "node",
        "Symbol",
        "src/new_lib.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_node_b = GraphRecord::symbol(
        sym_id_b.clone(),
        "fn",
        "src/new_lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository".to_owned(), // Identical summary!
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
        old_file_id.clone(),
        sym_id_a,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );
    let defines_edge_b = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        new_file_id.clone(),
        sym_id_b,
        Some("1.0".to_owned()),
        "file defines symbol b".to_owned(),
    );

    let contains_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        old_file_id,
        None,
        "contains file a".to_owned(),
    );
    let contains_edge_b = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        new_file_id,
        None,
        "contains file b".to_owned(),
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

    // Change 1: in commit 1, modifying src/old_lib.rs
    let change1_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit1_sha",
        "A",
        "src/old_lib.rs",
    ]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/old_lib.rs".to_owned()),
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

    // Change 2: in commit 2, renaming src/old_lib.rs to src/new_lib.rs
    let del_change_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit2_sha",
        "D",
        "src/old_lib.rs",
    ]);
    let change2_d = GraphRecord::node(
        del_change_id,
        NodeKind::Change,
        Some("src/old_lib.rs".to_owned()),
        None,
        None,
        "change 2 d".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let add_change_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit2_sha",
        "A",
        "src/new_lib.rs",
    ]);
    let change2_a = GraphRecord::node(
        add_change_id,
        NodeKind::Change,
        Some("src/new_lib.rs".to_owned()),
        None,
        None,
        "change 2 a".to_owned(),
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
    graph.push(old_file_node);
    graph.push(new_file_node);
    graph.push(sym_node_a);
    graph.push(sym_node_b);
    graph.push(defines_edge_a);
    graph.push(defines_edge_b);
    graph.push(contains_edge_a);
    graph.push(contains_edge_b);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(commit1);
    graph.push(change1);
    graph.push(commit2);
    graph.push(change2_d);
    graph.push(change2_a);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn test_query_who_handles_duplicate_symbol_names_in_different_files() {
    let (_temp, graph_path) = fixture_graph_for_duplicate_symbol_names();

    // Querying duplicate_symbol: it resolves to src/a.rs (Alice) first.
    // It should follow src/a.rs's history back to Commit 1 (Alice)
    // and ignore Commit 2 (Bob)'s changes to src/b.rs and src/a.rs.
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
    assert!(
        text.contains(
            "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
        ),
        "Expected last change by Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn fixture_graph_for_duplicate_symbol_names() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let repo_id = stable_id(&["node", "Repository", "my_repo"]);

    // Commit 2 is HEAD
    let repo_node = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("my_repo".to_owned()),
        "Repository my_repo".to_owned(),
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit2_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-02T12:00:00Z".to_owned(),
    });

    let file_id_a = stable_id(&["node", "File", "src/a.rs"]);
    let file_id_c = stable_id(&["node", "File", "src/c.rs"]);

    let file_node_a = GraphRecord::syntax_node(
        file_id_a.clone(),
        NodeKind::File,
        "src/a.rs".to_owned(),
        span(1, 50),
        "a.rs".to_owned(),
        "rust",
        "Source file src/a.rs".to_owned(),
    );

    let file_node_c = GraphRecord::syntax_node(
        file_id_c.clone(),
        NodeKind::File,
        "src/c.rs".to_owned(),
        span(1, 50),
        "c.rs".to_owned(),
        "rust",
        "Source file src/c.rs".to_owned(),
    );

    // Symbol node at Commit 1 in src/a.rs (Alice)
    let sym_id_a1 = stable_id(&[
        "node",
        "Symbol",
        "src/a.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_node_a1 = GraphRecord::symbol(
        sym_id_a1.clone(),
        "fn",
        "src/a.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in a.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Symbol node at Commit 2 in src/a.rs (re-emitted unchanged)
    let sym_id_a2 = stable_id(&[
        "node",
        "Symbol",
        "src/a.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_node_a2 = GraphRecord::symbol(
        sym_id_a2.clone(),
        "fn",
        "src/a.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in a.rs".to_owned(), // identical summary
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Symbol node at Commit 2 in src/c.rs (Bob)
    let sym_id_c2 = stable_id(&[
        "node",
        "Symbol",
        "src/c.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_node_c2 = GraphRecord::symbol(
        sym_id_c2.clone(),
        "fn",
        "src/c.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in c.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_a1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id_a.clone(),
        sym_id_a1,
        Some("1.0".to_owned()),
        "defines a1".to_owned(),
    );
    let defines_edge_a2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id_a.clone(),
        sym_id_a2,
        Some("1.0".to_owned()),
        "defines a2".to_owned(),
    );
    let defines_edge_c2 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id_c.clone(),
        sym_id_c2,
        Some("1.0".to_owned()),
        "defines c2".to_owned(),
    );

    let contains_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_id_a,
        None,
        "contains a".to_owned(),
    );
    let contains_edge_c = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_id_c,
        None,
        "contains c".to_owned(),
    );

    // Commit 1: Alice
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

    // Change 1: modifying src/a.rs in commit 1
    let change1_id = stable_id(&["node", "Change", "my_repo", "commit1_sha", "A", "src/a.rs"]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/a.rs".to_owned()),
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

    // Commit 2: Bob
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

    // Change 2a: modifying src/a.rs in commit 2
    let change2a_id = stable_id(&["node", "Change", "my_repo", "commit2_sha", "M", "src/a.rs"]);
    let change2a = GraphRecord::node(
        change2a_id,
        NodeKind::Change,
        Some("src/a.rs".to_owned()),
        None,
        None,
        "change 2 a".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Change 2c: modifying src/c.rs in commit 2
    let change2c_id = stable_id(&["node", "Change", "my_repo", "commit2_sha", "A", "src/c.rs"]);
    let change2c = GraphRecord::node(
        change2c_id,
        NodeKind::Change,
        Some("src/c.rs".to_owned()),
        None,
        None,
        "change 2 c".to_owned(),
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
    graph.push(file_node_a);
    graph.push(file_node_c);
    graph.push(sym_node_a1);
    graph.push(sym_node_a2);
    graph.push(sym_node_c2);
    graph.push(defines_edge_a1);
    graph.push(defines_edge_a2);
    graph.push(defines_edge_c2);
    graph.push(contains_edge_a);
    graph.push(contains_edge_c);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(commit1);
    graph.push(change1);
    graph.push(commit2);
    graph.push(change2a);
    graph.push(change2c);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn test_query_who_scoped_repo_resolves_prefix_with_unowned_collision() {
    let (_temp, graph_path) =
        fixture_graph_for_scoped_repo_resolves_prefix_with_unowned_collision();

    // Querying with --repo my_repo --at commit1_sh (which shares prefix with unowned commit1_sha_other).
    // It should succeed because commit1_sha_other is unowned and thus excluded when --repo is active.
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("my_repo")
        .arg("--at")
        .arg("commit1_sh")
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
            "scan_repository last changed by Alice <alice@example.com> in commit commit1_sha"
        ),
        "Expected Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn fixture_graph_for_scoped_repo_resolves_prefix_with_unowned_collision()
-> (tempfile::TempDir, PathBuf) {
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

    let file_id = stable_id(&["node", "File", "src/a.rs"]);
    let file_node = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/a.rs".to_owned(),
        span(1, 50),
        "a.rs".to_owned(),
        "rust",
        "Source file src/a.rs".to_owned(),
    );

    let sym_id = stable_id(&[
        "node",
        "Symbol",
        "src/a.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_node = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/a.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in a.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        sym_id,
        Some("1.0".to_owned()),
        "defines".to_owned(),
    );

    let contains_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_id,
        None,
        "contains".to_owned(),
    );

    // Commit 1 (owned by my_repo)
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

    let contains_commit1 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id,
        commit1.id().to_owned(),
        None,
        "contains commit 1".to_owned(),
    );

    // Unowned commit (shares prefix "commit1_sh")
    let commit1_other_id = stable_id(&["node", "Commit", "other_repo", "commit1_sha_other"]);
    let commit1_other = GraphRecord::node(
        commit1_other_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit1_sha_other".to_owned()),
        "Commit 1 Other".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha_other".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(Some("Bob".to_owned()), Some("bob@example.com".to_owned()));

    let change1_id = stable_id(&["node", "Change", "my_repo", "commit1_sha", "M", "src/a.rs"]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/a.rs".to_owned()),
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

    let mut graph = Graph::new();
    graph.push(repo_node);
    graph.push(file_node);
    graph.push(sym_node);
    graph.push(defines_edge);
    graph.push(contains_edge);
    graph.push(commit1);
    graph.push(contains_commit1);
    graph.push(commit1_other);
    graph.push(change1);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn test_query_who_liveness_requires_matching_symbol_identity() {
    let (_temp, graph_path) = fixture_graph_for_liveness_requires_matching_symbol_identity();

    // Querying for 'scan_repository' (which was deleted in a.rs, but another 'scan_repository' exists in b.rs at HEAD).
    // The query should succeed and resolve to b.rs (since a.rs is not live and thus filtered out).
    // Under the old name-only liveness check, both would be kept, causing an ambiguity error.
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("my_repo")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains("src/b.rs"),
        "Expected resolved path to be src/b.rs, but got:\n{text}"
    );
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn fixture_graph_for_liveness_requires_matching_symbol_identity() -> (tempfile::TempDir, PathBuf) {
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
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit2_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-02T12:00:00Z".to_owned(),
    });

    let file_a_id = stable_id(&["node", "File", "src/a.rs"]);
    let file_a_node = GraphRecord::syntax_node(
        file_a_id.clone(),
        NodeKind::File,
        "src/a.rs".to_owned(),
        span(1, 50),
        "a.rs".to_owned(),
        "rust",
        "Source file src/a.rs".to_owned(),
    );

    let file_b_id = stable_id(&["node", "File", "src/b.rs"]);
    let file_b_node = GraphRecord::syntax_node(
        file_b_id.clone(),
        NodeKind::File,
        "src/b.rs".to_owned(),
        span(1, 50),
        "b.rs".to_owned(),
        "rust",
        "Source file src/b.rs".to_owned(),
    );

    let contains_a_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_a_id.clone(),
        None,
        "contains a".to_owned(),
    );

    let contains_b_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_b_id.clone(),
        None,
        "contains b".to_owned(),
    );

    // Symbol node in a.rs (only exists at commit1)
    let sym_a_id = stable_id(&[
        "node",
        "Symbol",
        "src/a.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_a_node = GraphRecord::symbol(
        sym_a_id.clone(),
        "fn",
        "src/a.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in a.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit1_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Symbol node in b.rs (exists at commit2/HEAD)
    let sym_b_id = stable_id(&[
        "node",
        "Symbol",
        "src/b.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_b_node = GraphRecord::symbol(
        sym_b_id.clone(),
        "fn",
        "src/b.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository in b.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_a_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_a_id,
        sym_a_id,
        Some("1.0".to_owned()),
        "defines a".to_owned(),
    );

    let defines_b_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_b_id,
        sym_b_id,
        Some("1.0".to_owned()),
        "defines b".to_owned(),
    );

    // Commit 1: Alice
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

    // Commit 2: Bob (HEAD)
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

    // Changes:
    let change1_id = stable_id(&["node", "Change", "my_repo", "commit1_sha", "M", "src/a.rs"]);
    let change1 = GraphRecord::node(
        change1_id,
        NodeKind::Change,
        Some("src/a.rs".to_owned()),
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

    let change2_id = stable_id(&["node", "Change", "my_repo", "commit2_sha", "M", "src/b.rs"]);
    let change2 = GraphRecord::node(
        change2_id,
        NodeKind::Change,
        Some("src/b.rs".to_owned()),
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

    let mut graph = Graph::new();
    graph.push(repo_node);
    graph.push(file_a_node);
    graph.push(file_b_node);
    graph.push(contains_a_edge);
    graph.push(contains_b_edge);
    graph.push(sym_a_node);
    graph.push(sym_b_node);
    graph.push(defines_a_edge);
    graph.push(defines_b_edge);
    graph.push(commit1);
    graph.push(commit2);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(change1);
    graph.push(change2);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

#[test]
fn test_query_who_as_of_scopes_lineage_to_resolved_symbol_commit() {
    let (_temp, graph_path) = fixture_graph_for_as_of_scopes_lineage_to_resolved_symbol_commit();

    // Querying for 'scan_repository' as of 2026-01-01T12:00:00Z.
    // The active resolved symbol is at commit1_sha (Alice).
    // Sibling branch commit2_sha (Bob) is merged later into commit3_sha (HEAD).
    // The query must attribute the change to Alice (commit1_sha), not Bob (commit2_sha),
    // because Bob's commit is on a sibling branch that is NOT an ancestor of commit1_sha.
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("my_repo")
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
    assert!(
        text.contains("last changed by Alice <alice@example.com> in commit commit1_sha"),
        "Expected Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn fixture_graph_for_as_of_scopes_lineage_to_resolved_symbol_commit() -> (tempfile::TempDir, PathBuf)
{
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
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit3_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-03T12:00:00Z".to_owned(),
    });

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

    let contains_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_id.clone(),
        None,
        "contains".to_owned(),
    );

    // Commit 1 (Alice, main branch)
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

    let sym_a_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_a_node = GraphRecord::symbol(
        sym_a_id,
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

    let defines_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit1_sha",
        ]),
        Some("1.0".to_owned()),
        "defines a".to_owned(),
    );

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

    // Commit 2 (Bob, side branch)
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
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(Some("Bob".to_owned()), Some("bob@example.com".to_owned()));

    let sym_b_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_b_node = GraphRecord::symbol(
        sym_b_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_b = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit2_sha",
        ]),
        Some("1.0".to_owned()),
        "defines b".to_owned(),
    );

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
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Commit 3 (Charlie, merge commit HEAD merging 1 and 2)
    let commit3_id = stable_id(&["node", "Commit", "my_repo", "commit3_sha"]);
    let commit3 = GraphRecord::node(
        commit3_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit3_sha".to_owned()),
        "Commit 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned(), "commit2_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(
        Some("Charlie".to_owned()),
        Some("charlie@example.com".to_owned()),
    );

    let sym_c_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit3_sha",
    ]);
    let sym_c_node = GraphRecord::symbol(
        sym_c_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned(), "commit2_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_c = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id,
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit3_sha",
        ]),
        Some("1.0".to_owned()),
        "defines c".to_owned(),
    );

    let change3_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit3_sha",
        "M",
        "src/lib.rs",
    ]);
    let change3 = GraphRecord::node(
        change3_id,
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "change 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned(), "commit2_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
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
        repo_id.clone(),
        commit2.id().to_owned(),
        None,
        "contains commit 2".to_owned(),
    );

    let contains_commit3 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id,
        commit3.id().to_owned(),
        None,
        "contains commit 3".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(repo_node);
    graph.push(file_node);
    graph.push(contains_edge);
    graph.push(sym_a_node);
    graph.push(sym_b_node);
    graph.push(sym_c_node);
    graph.push(defines_edge_a);
    graph.push(defines_edge_b);
    graph.push(defines_edge_c);
    graph.push(commit1);
    graph.push(commit2);
    graph.push(commit3);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(contains_commit3);
    graph.push(change1);
    graph.push(change2);
    graph.push(change3);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn test_query_who_as_of_excludes_unmerged_sibling_branch() {
    let (_temp, graph_path) = fixture_graph_with_unmerged_sibling_branch();

    // Querying for 'scan_repository' as of 2026-01-02T12:00:00Z.
    // The unmerged sibling branch commit2_sha (Bob) is newer than commit1_sha (Alice),
    // but it is not in the lineage of HEAD (commit3_sha).
    // The query must exclude Bob and attribute the change to Alice (commit1_sha).
    let output = egregore()
        .args(["query", "who", "scan_repository", "--graph"])
        .arg(&graph_path)
        .arg("--repo")
        .arg("my_repo")
        .arg("--as-of")
        .arg("2026-01-02T12:00:00Z")
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains("last changed by Alice <alice@example.com> in commit commit1_sha"),
        "Expected Alice in commit1_sha, but output was:\n{text}"
    );
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn fixture_graph_with_unmerged_sibling_branch() -> (tempfile::TempDir, PathBuf) {
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
    )
    .with_source_snapshot(aletheia_egregore::SourceSnapshotPayload {
        head: aletheia_egregore::SnapshotHead::Commit {
            sha: "commit3_sha".to_owned(),
        },
        dirty: false,
        repository_id: repo_id.clone(),
        scanned_at: "2026-01-03T12:00:00Z".to_owned(),
    });

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

    let contains_edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id.clone(),
        file_id.clone(),
        None,
        "contains".to_owned(),
    );

    // Commit 1 (Alice, main branch)
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

    let sym_a_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit1_sha",
    ]);
    let sym_a_node = GraphRecord::symbol(
        sym_a_id,
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

    let defines_edge_a = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit1_sha",
        ]),
        Some("1.0".to_owned()),
        "defines a".to_owned(),
    );

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

    // Commit 2 (Bob, unmerged sibling branch)
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
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(Some("Bob".to_owned()), Some("bob@example.com".to_owned()));

    let sym_b_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit2_sha",
    ]);
    let sym_b_node = GraphRecord::symbol(
        sym_b_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 2".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit2_sha".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_b = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit2_sha",
        ]),
        Some("1.0".to_owned()),
        "defines b".to_owned(),
    );

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
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: Some("2026-01-02T00:00:00Z".to_owned()),
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Commit 3 (Charlie, HEAD on main, only has parent commit1_sha)
    let commit3_id = stable_id(&["node", "Commit", "my_repo", "commit3_sha"]);
    let commit3 = GraphRecord::node(
        commit3_id,
        NodeKind::Commit,
        None,
        None,
        Some("commit3_sha".to_owned()),
        "Commit 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_author(
        Some("Charlie".to_owned()),
        Some("charlie@example.com".to_owned()),
    );

    let sym_c_id = stable_id(&[
        "node",
        "Symbol",
        "src/lib.rs",
        "scan_repository",
        "commit3_sha",
    ]);
    let sym_c_node = GraphRecord::symbol(
        sym_c_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let defines_edge_c = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id,
        stable_id(&[
            "node",
            "Symbol",
            "src/lib.rs",
            "scan_repository",
            "commit3_sha",
        ]),
        Some("1.0".to_owned()),
        "defines c".to_owned(),
    );

    let change3_id = stable_id(&[
        "node",
        "Change",
        "my_repo",
        "commit3_sha",
        "M",
        "src/lib.rs",
    ]);
    let change3 = GraphRecord::node(
        change3_id,
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "change 3".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "commit3_sha".to_owned(),
        git_parent_commits: vec!["commit1_sha".to_owned()],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: Some("2026-01-03T00:00:00Z".to_owned()),
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
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
        repo_id.clone(),
        commit2.id().to_owned(),
        None,
        "contains commit 2".to_owned(),
    );

    let contains_commit3 = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id,
        commit3.id().to_owned(),
        None,
        "contains commit 3".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(repo_node);
    graph.push(file_node);
    graph.push(contains_edge);
    graph.push(sym_a_node);
    graph.push(sym_b_node);
    graph.push(sym_c_node);
    graph.push(defines_edge_a);
    graph.push(defines_edge_b);
    graph.push(defines_edge_c);
    graph.push(commit1);
    graph.push(commit2);
    graph.push(commit3);
    graph.push(contains_commit1);
    graph.push(contains_commit2);
    graph.push(contains_commit3);
    graph.push(change1);
    graph.push(change2);
    graph.push(change3);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}
