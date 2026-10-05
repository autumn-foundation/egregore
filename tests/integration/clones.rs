//! Integration tests for `eg query clones` (issue #216): group exact-duplicate
//! Rust symbol bodies into citable clone classes.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command as CargoCommand;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

/// Seeds a fixture crate: `src/a.rs` and `src/b.rs` carry the same normalized
/// function body (different formatting and comments); `src/c.rs` is distinct.
fn seed_clone_repo(repo: &Path) {
    write(
        repo,
        "src/a.rs",
        "pub fn compute(x: u64) -> u64 {\n    // double it\n    x * 2\n}\n",
    );
    write(
        repo,
        "src/b.rs",
        "// same math, one line\npub fn compute(x: u64) -> u64 { x * 2 }\n",
    );
    write(
        repo,
        "src/c.rs",
        "pub fn other(x: u64) -> u64 {\n    x + 1\n}\n",
    );
}

/// Seeds a fixture crate with no duplicates: every function body is distinct.
fn seed_distinct_repo(repo: &Path) {
    write(repo, "src/a.rs", "pub fn alpha() -> u64 {\n    1\n}\n");
    write(repo, "src/b.rs", "pub fn beta() -> u64 {\n    2\n}\n");
}

/// Scans a fixture repo directory into graph JSONL; returns the owning temp
/// dir and the graph path.
fn scan_fixture_repo(repo: &Path) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo_dir = temp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("repo dir should be created");
    for entry in fs::read_dir(repo).expect("fixture repo should be readable") {
        let entry = entry.expect("dir entry");
        let dest = repo_dir.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            fs_extra_copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), &dest).expect("copy fixture file");
        }
    }
    let graph = temp.path().join("code.graph.jsonl");
    egregore()
        .args(["scan"])
        .arg(&repo_dir)
        .args(["--out"])
        .arg(&graph)
        .assert()
        .success();
    (temp, graph)
}

fn fs_extra_copy_dir(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).expect("dest dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            fs_extra_copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

fn run_clones_json(graph: &Path, extra: &[&str]) -> serde_json::Value {
    let output = egregore()
        .args(["query", "clones", "--graph"])
        .arg(graph)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    serde_json::from_str(stdout.trim()).expect("stdout should be one JSON envelope")
}

fn run_clones_text(graph: &Path, extra: &[&str]) -> String {
    let output = egregore()
        .args(["query", "clones", "--graph"])
        .arg(graph)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("stdout should be utf-8")
}

#[test]
fn clones_groups_exact_duplicates_across_files() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let parsed = run_clones_json(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];
    assert_eq!(result["min_size"], 2);
    assert_eq!(result["truncated"], false);
    let classes = result["classes"].as_array().expect("classes array");
    assert_eq!(classes.len(), 1, "one duplicate pair forms one class");
    let class = &classes[0];
    assert_eq!(class["size"], 2);
    let hash = class["content_hash"].as_str().expect("hash string");
    assert!(
        hash.starts_with("blake3:"),
        "class identity carries the blake3: framing"
    );
    let members = class["members"].as_array().expect("members array");
    let mut paths: Vec<&str> = members
        .iter()
        .map(|m| m["repo_relative_path"].as_str().expect("path"))
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["src/a.rs", "src/b.rs"]);
    // Members are ordered by record ID; verify that ordering separately.
    let ids: Vec<&str> = members
        .iter()
        .map(|m| m["record_id"].as_str().expect("record id"))
        .collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    assert_eq!(ids, sorted_ids, "members are ordered by record ID");
    for member in members {
        assert!(
            member["record_id"]
                .as_str()
                .expect("record id")
                .starts_with("codegraph:"),
            "member carries a citable record id"
        );
        assert!(
            !member["qualified_name"].as_str().unwrap_or("").is_empty(),
            "member carries a qualified name"
        );
        assert!(
            member["span"]["start_line"].as_u64().unwrap_or(0) >= 1,
            "member carries a 1-based span"
        );
    }
}

#[test]
fn clones_output_never_carries_raw_source_bodies() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let stdout = run_clones_text(&graph, &[]);
    // The shared body fragment must not leak through either format; the text
    // renderer prints handles only.
    assert!(
        !stdout.contains("x * 2"),
        "text output must not contain the raw duplicated body"
    );
    let parsed = run_clones_json(&graph, &[]);
    let envelope = serde_json::to_string(&parsed).expect("envelope serializes");
    assert!(
        !envelope.contains("x * 2"),
        "JSON output must not contain the raw duplicated body"
    );
}

#[test]
fn clones_empty_scope_returns_wellformed_empty_result() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_distinct_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let parsed = run_clones_json(&graph, &[]);
    assert_eq!(parsed["ok"], true, "empty scope is a successful answer");
    let result = &parsed["result"];
    assert_eq!(
        result["classes"].as_array().expect("classes array").len(),
        0
    );
    assert_eq!(result["empty_reason"], "no_clone_classes");
    assert_eq!(result["truncated"], false);
}

#[test]
fn clones_is_deterministic_across_runs() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let first = run_clones_text(&graph, &[]);
    let second = run_clones_text(&graph, &[]);
    assert_eq!(first, second, "repeated runs are byte-identical");
    let first_json = run_clones_json(&graph, &[]);
    let second_json = run_clones_json(&graph, &[]);
    assert_eq!(first_json, second_json);
}

#[test]
fn clones_min_size_filters_classes() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let parsed = run_clones_json(&graph, &["--min-size", "3"]);
    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];
    assert_eq!(result["min_size"], 3);
    assert_eq!(
        result["classes"].as_array().expect("classes array").len(),
        0,
        "--min-size 3 hides the size-2 class"
    );
    assert_eq!(result["empty_reason"], "no_clone_classes");
}

#[test]
fn clones_limit_truncates_with_explicit_signal() {
    // Eight files form four duplicate pairs; --limit 3 must return three
    // classes and say so explicitly.
    let fixture = tempfile::tempdir().expect("temp dir");
    for i in 0..4 {
        write(
            fixture.path(),
            &format!("src/pair{i}_a.rs"),
            &format!("pub fn dup() -> u64 {{\n    // class {i}\n    {i}\n}}\n"),
        );
        write(
            fixture.path(),
            &format!("src/pair{i}_b.rs"),
            &format!("pub fn dup() -> u64 {{ {i} }} // class {i}\n"),
        );
    }
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let parsed = run_clones_json(&graph, &["--limit", "3"]);
    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];
    assert_eq!(result["total_class_count"], 4);
    assert_eq!(result["returned_class_count"], 3);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["classes"].as_array().expect("classes").len(), 3);

    let text = run_clones_text(&graph, &["--limit", "3", "--format", "text"]);
    assert!(
        text.contains("truncated: showing 3 of 4 classes"),
        "text format carries the explicit truncation line"
    );
}

#[test]
fn clones_rejects_invalid_bounds() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    let stderr = String::from_utf8(
        egregore()
            .args(["query", "clones", "--graph"])
            .arg(&graph)
            .args(["--limit", "0"])
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone(),
    )
    .expect("stderr utf-8");
    assert!(stderr.contains("invalid_limit"), "limit 0 is rejected");

    let stderr = String::from_utf8(
        egregore()
            .args(["query", "clones", "--graph"])
            .arg(&graph)
            .args(["--min-size", "1"])
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone(),
    )
    .expect("stderr utf-8");
    assert!(
        stderr.contains("invalid_min_size"),
        "min-size 1 is rejected"
    );
}

#[test]
fn clones_unknown_repo_selector_fails() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (_temp, graph) = scan_fixture_repo(fixture.path());

    egregore()
        .args(["query", "clones", "--graph"])
        .arg(&graph)
        .args(["--repo", "no-such-repo"])
        .assert()
        .failure();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn clones_embedded_data_dir_matches_graph() {
    let fixture = tempfile::tempdir().expect("temp dir");
    seed_clone_repo(fixture.path());
    let (temp, graph) = scan_fixture_repo(fixture.path());

    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let output = egregore()
        .args(["query", "clones", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let embedded: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).expect("utf-8").trim())
            .expect("one JSON envelope");
    let from_graph = run_clones_json(&graph, &[]);

    assert_eq!(embedded["ok"], true);
    assert_eq!(
        embedded["result"]["classes"], from_graph["result"]["classes"],
        "embedded store returns the same classes as the graph JSONL path"
    );
}

#[test]
fn clones_repo_selector_scopes_to_one_repository() {
    // Two repos, each with its own duplicate pair. Concatenated into one
    // graph, --repo must isolate each repo's clone classes.
    let temp = tempfile::tempdir().expect("temp dir");

    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(repo_a.join("src")).expect("repo-a src");
    fs::create_dir_all(repo_b.join("src")).expect("repo-b src");
    // repo-a: duplicate pair with body "x * 2".
    write(
        &repo_a,
        "src/a.rs",
        "pub fn compute(x: u64) -> u64 {\n    x * 2\n}\n",
    );
    write(
        &repo_a,
        "src/b.rs",
        "pub fn compute(x: u64) -> u64 { x * 2 }\n",
    );
    // repo-b: duplicate pair with a DIFFERENT body ("x * 3").
    write(
        &repo_b,
        "src/c.rs",
        "pub fn triple(x: u64) -> u64 {\n    x * 3\n}\n",
    );
    write(
        &repo_b,
        "src/d.rs",
        "pub fn triple(x: u64) -> u64 { x * 3 }\n",
    );

    let graph_a = temp.path().join("a.graph.jsonl");
    let graph_b = temp.path().join("b.graph.jsonl");
    egregore()
        .args(["scan"])
        .arg(&repo_a)
        .args(["--out"])
        .arg(&graph_a)
        .assert()
        .success();
    egregore()
        .args(["scan"])
        .arg(&repo_b)
        .args(["--out"])
        .arg(&graph_b)
        .assert()
        .success();

    // Concatenate into a shared multi-repo store.
    let combined = temp.path().join("combined.graph.jsonl");
    let mut out = fs::read(&graph_a).expect("read graph a");
    out.extend(fs::read(&graph_b).expect("read graph b"));
    fs::write(&combined, out).expect("write combined graph");

    // Unscoped: both classes appear.
    let parsed = run_clones_json(&combined, &[]);
    assert_eq!(
        parsed["result"]["classes"]
            .as_array()
            .expect("classes")
            .len(),
        2,
        "unscoped query sees both repos' classes"
    );

    // Scoped to repo-a: only repo-a's pair. The two repos' pairs have
    // different bodies, hence different class hashes.
    let parsed_a = run_clones_json(&combined, &["--repo", "repo-a"]);
    let classes_a = parsed_a["result"]["classes"]
        .as_array()
        .expect("classes array");
    assert_eq!(classes_a.len(), 1, "repo-a scope isolates one class");
    let hash_a = classes_a[0]["content_hash"].as_str().expect("hash");

    // Scoped to repo-b: only repo-b's pair, a different class.
    let parsed_b = run_clones_json(&combined, &["--repo", "repo-b"]);
    let classes_b = parsed_b["result"]["classes"]
        .as_array()
        .expect("classes array");
    assert_eq!(classes_b.len(), 1, "repo-b scope isolates one class");
    let hash_b = classes_b[0]["content_hash"].as_str().expect("hash");

    assert_ne!(
        hash_a, hash_b,
        "the two repos' classes are distinct (different bodies)"
    );
    let unscoped_hashes: Vec<&str> = parsed["result"]["classes"]
        .as_array()
        .expect("classes")
        .iter()
        .map(|c| c["content_hash"].as_str().expect("hash"))
        .collect();
    assert!(
        unscoped_hashes.contains(&hash_a) && unscoped_hashes.contains(&hash_b),
        "unscoped query contains both repos' classes"
    );
}
