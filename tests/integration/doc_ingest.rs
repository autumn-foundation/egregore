//! CLI integration tests for `eg import docs` and the additive design-doc rows
//! in `eg query symbol` (issue #149).
//!
//! The tests build a scratch repo from `tests/fixtures/rust_basic`, write
//! design docs that reference the scanned graph, and drive the binary
//! end to end: scan → import docs → (optionally) query symbol.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURE: &str = "tests/fixtures/rust_basic";
const TX: &str = "2026-09-28T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

/// Copies the fixture into a fresh repo dir and writes `docs` files.
/// Returns the repo tempdir and root path.
fn repo_with_docs(docs: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("temp dir");
    let root = tmp.path().join("repo");
    copy_fixture(&root);
    for (rel, content) in docs {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("doc parent")).expect("doc dirs");
        fs::write(&path, content).expect("write doc");
    }
    (tmp, root)
}

fn copy_fixture(root: &Path) {
    let src = Path::new(FIXTURE).join("src").join("lib.rs");
    let dst_dir = root.join("src");
    fs::create_dir_all(&dst_dir).expect("src dir");
    fs::copy(src, dst_dir.join("lib.rs")).expect("copy fixture");
}

fn scan_repo(root: &Path, out: &Path) {
    egregore()
        .args(["scan"])
        .arg(root)
        .args(["--out"])
        .arg(out)
        .assert()
        .success();
}

/// Reads a JSONL graph file; returns two distinct Symbol names and the
/// first File's repo-relative path.
fn symbol_names_and_file(graph: &Path) -> (String, String, String) {
    let content = fs::read_to_string(graph).expect("graph readable");
    let mut symbols: Vec<String> = Vec::new();
    let mut file: Option<String> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).expect("valid JSONL");
        if v["type"] == "node" || v.get("kind").is_some() {
            match v["kind"].as_str() {
                Some("Symbol") => {
                    if let Some(name) = v["name"].as_str().map(str::to_owned)
                        && !symbols.contains(&name)
                    {
                        symbols.push(name);
                    }
                }
                Some("File") if file.is_none() => {
                    file = v["repo_relative_path"].as_str().map(str::to_owned);
                }
                _ => {}
            }
        }
    }
    assert!(symbols.len() >= 2, "graph needs two symbols");
    let second = symbols[1].clone();
    let first = symbols.remove(0);
    (first, second, file.expect("graph has a file"))
}

/// Reads a JSONL graph file; returns the first Symbol's (name, id) and the
/// first `File`'s (`repo_relative_path`, id).
fn first_symbol_and_file(graph: &Path) -> (String, String) {
    let (first, _, file) = symbol_names_and_file(graph);
    (first, file)
}

fn import_docs(root: &Path, graph: &Path, out: &Path) -> assert_cmd::assert::Assert {
    egregore()
        .args(["import", "docs"])
        .args(["--repo-root"])
        .arg(root)
        .args(["--code-graph"])
        .arg(graph)
        .args(["--out"])
        .arg(out)
        .args(["--transaction-time", TX])
        .assert()
}

fn read_records(path: &Path) -> Vec<Value> {
    let content = fs::read_to_string(path).expect("output readable");
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL record"))
        .collect()
}

#[test]
fn import_docs_happy_path_writes_node_and_edges_and_is_byte_identical() {
    let (_tmp, root) = repo_with_docs(&[]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);
    let (symbol_name, file_path) = first_symbol_and_file(&graph_path);

    let doc = format!(
        "---\ntitle: Widget decision\n---\n# Widget decision\n\nUses `{file_path}` and the `{symbol_name}` symbol.\n"
    );
    let doc_path = root.join("docs").join("adr").join("0001-widget.md");
    fs::create_dir_all(doc_path.parent().unwrap()).unwrap();
    fs::write(&doc_path, doc).unwrap();

    let out = root.join("docs.jsonl");
    import_docs(&root, &graph_path, &out).success();

    let records = read_records(&out);
    assert_eq!(records.len(), 3, "one node + two edges: {records:?}");
    let node = &records[0];
    assert_eq!(node["kind"], "ADR");
    assert_eq!(node["name"], "Widget decision");
    assert!(node["id"].as_str().unwrap().starts_with("artifact:v1:"));
    assert_eq!(node["producer"]["producer_kind"], "doc_importer");
    assert!(node["source_artifact_hash"].as_str().is_some());
    let labels: Vec<&str> = records[1..]
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"RELATES_TO"));
    assert!(labels.contains(&"MENTIONS_SYMBOL"));

    // Re-import must be byte-identical.
    let out2 = root.join("docs2.jsonl");
    import_docs(&root, &graph_path, &out2).success();
    assert_eq!(
        fs::read(&out).unwrap(),
        fs::read(&out2).unwrap(),
        "re-import is byte-identical"
    );
}

#[test]
fn import_docs_unresolved_reference_diagnostic_exit_2_and_no_body_leak() {
    let doc = "---\ntitle: Ghost ref\n---\n# Ghost ref\n\nReferences `no/such/file.rs` with secret hunter2-token.\n";
    let (_tmp, root) = repo_with_docs(&[("docs/adr/0001-ghost.md", doc)]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);

    let out = root.join("docs.jsonl");
    let assert = import_docs(&root, &graph_path, &out).code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(
        stderr.contains("unresolved_reference"),
        "stderr names the diagnostic: {stderr}"
    );
    assert!(
        !stderr.contains("hunter2-token"),
        "raw body text never leaks into diagnostics"
    );
    // No records: the only document had zero resolvable references.
    assert_eq!(read_records(&out).len(), 0);
}

/// A symlinked directory under a documented root must not be followed: it could
/// import Markdown from outside the repository under a synthetic repo-relative
/// path, or recurse forever through a link to an ancestor.
#[cfg(unix)]
#[test]
fn import_docs_does_not_follow_symlinked_directories() {
    let (tmp, root) = repo_with_docs(&[]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);
    let (symbol_name, file_path) = first_symbol_and_file(&graph_path);

    // An outside directory holding a doc that WOULD resolve if it were imported.
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("0002-outside.md"),
        format!("---\ntitle: Outside\n---\n# Outside\n\nUses `{file_path}` and `{symbol_name}`.\n"),
    )
    .unwrap();
    let adr = root.join("docs").join("adr");
    fs::create_dir_all(&adr).unwrap();
    std::os::unix::fs::symlink(&outside, adr.join("linked-outside")).unwrap();
    // A link back to an ancestor: following it would never terminate.
    std::os::unix::fs::symlink(&root, adr.join("loop")).unwrap();

    let out = root.join("docs.jsonl");
    let assert = import_docs(&root, &graph_path, &out);
    let output = assert.get_output();
    let records = read_records(&out);
    assert!(
        records.iter().all(|r| !r.to_string().contains("Outside")),
        "outside docs must not be imported, got {records:?} (exit {:?})",
        output.status.code()
    );
}

#[test]
fn import_docs_unknown_root_exit_2() {
    let (_tmp, root) = repo_with_docs(&[]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);

    let out = root.join("docs.jsonl");
    let assert = egregore()
        .args(["import", "docs"])
        .args(["--repo-root"])
        .arg(&root)
        .args(["--code-graph"])
        .arg(&graph_path)
        .args(["--root", "docs/nope"])
        .args(["--out"])
        .arg(&out)
        .args(["--transaction-time", TX])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("unknown_root"), "stderr: {stderr}");
}

#[test]
fn import_docs_empty_document_diagnostic_exit_2() {
    let (_tmp, root) = repo_with_docs(&[("docs/prd/empty.md", "\n   \n")]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);

    let out = root.join("docs.jsonl");
    let assert = import_docs(&root, &graph_path, &out).code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("empty_document"), "stderr: {stderr}");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_surfaces_design_doc_row() {
    let (_tmp, root) = repo_with_docs(&[]);
    let graph_path = root.join("graph.jsonl");
    scan_repo(&root, &graph_path);
    let (symbol_name, other_symbol, _file_path) = symbol_names_and_file(&graph_path);

    let doc = format!(
        "---\ntitle: Widget decision\n---\n# Widget decision\n\nThe `{symbol_name}` symbol is load-bearing.\n"
    );
    let doc_path = root.join("docs").join("adr").join("0001-widget.md");
    fs::create_dir_all(doc_path.parent().unwrap()).unwrap();
    fs::write(&doc_path, doc).unwrap();

    let docs_path = root.join("docs.jsonl");
    import_docs(&root, &graph_path, &docs_path).success();

    // Combine code graph + docs into one graph for querying.
    let combined = root.join("combined.jsonl");
    let mut bytes = fs::read(&graph_path).unwrap();
    bytes.extend_from_slice(&fs::read(&docs_path).unwrap());
    fs::write(&combined, bytes).unwrap();

    let output = egregore()
        .args(["query", "symbol", &symbol_name, "--graph"])
        .arg(&combined)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        stdout.contains("design-doc:"),
        "query symbol prints the additive design-doc row:\n{stdout}"
    );
    assert!(
        stdout.contains("docs/adr/0001-widget.md"),
        "row carries the doc path:\n{stdout}"
    );
    assert!(stdout.contains("(ADR)"), "row carries the kind:\n{stdout}");

    // A symbol with no linked docs gets no design-doc rows (honest empty).
    // `other_symbol` is a real second symbol from the graph, so the query
    // itself succeeds — only the doc rows are absent.
    let output = egregore()
        .args(["query", "symbol", &other_symbol, "--graph"])
        .arg(&combined)
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf8");
    assert!(
        !stdout.contains("design-doc:"),
        "no invented doc rows for unlinked symbols:\n{stdout}"
    );
}
