//! `eg inspect --data-dir` — daemon-free embedded-store inspection (issue #125).
//!
//! These fixtures prove the acceptance criteria: direct embedded reads with no
//! daemon, per-domain/per-kind/per-schema-version trust-class counts, distinct
//! unknown-schema-version labeling, JSONL parity, strict read-only behavior with
//! byte-identical output across runs, redaction-safe output, and stable
//! operator-facing diagnostics for missing or empty stores.

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{
    GraphRecord, NodeKind, SCHEMA_VERSION,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION},
};

/// Sentinel narrative that must never leak into inspect output.
#[cfg(feature = "embedded-aletheiadb")]
const RAW_PAYLOAD_SENTINEL: &str = "RAW-OBSERVATION-PAYLOAD-DO-NOT-PRINT";

#[cfg(feature = "embedded-aletheiadb")]
fn node_with_version(
    id: &str,
    kind: NodeKind,
    schema_version: u32,
    name: &str,
    summary: &str,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        Some(name.to_owned()),
        summary.to_owned(),
    );
    if let GraphRecord::Node {
        schema_version: version,
        ..
    } = &mut record
    {
        *version = schema_version;
    }
    record
}

/// A small graph spanning three domains and three `(domain, kind, schema_version)`
/// tuples: codegraph Repository + File, `agent_memory` Observation, project Task.
#[cfg(feature = "embedded-aletheiadb")]
fn mixed_domain_jsonl() -> String {
    let records = vec![
        node_with_version(
            "codegraph:v5:test-repo",
            NodeKind::Repository,
            SCHEMA_VERSION,
            "test-repo",
            "test repo",
        ),
        node_with_version(
            "codegraph:v5:test-file",
            NodeKind::File,
            SCHEMA_VERSION,
            "src/lib.rs",
            "file record",
        ),
        node_with_version(
            "agent_memory:v1:obs-1",
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            "obs1",
            RAW_PAYLOAD_SENTINEL,
        ),
        node_with_version(
            "project:v1:task-1",
            NodeKind::Task,
            PROJECT_SCHEMA_VERSION,
            "task1",
            "project task record",
        ),
    ];
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_embedded(graph_path: &Path, data_dir: &Path) {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn inspect_data_dir_stdout(data_dir: &Path, format: Option<&str>) -> String {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("inspect").arg("--data-dir").arg(data_dir);
    if let Some(format) = format {
        cmd.arg("--format").arg(format);
    }
    let output = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(output).expect("inspect output should be UTF-8")
}

/// Recursive snapshot of every regular file under `dir`: relative path -> bytes.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("store dir should be readable") {
            let entry = entry.expect("store dir entry should be readable");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("entry should be under root")
                    .to_path_buf();
                let bytes = fs::read(&path).expect("store file should be readable");
                out.insert(relative, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_defaults_to_single_line_json_with_trust_class_counts() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = inspect_data_dir_stdout(&data_dir, None);

    // Newline-delimited JSON by default: exactly one JSON object on one line.
    assert!(
        stdout.ends_with('\n') && stdout.trim_end_matches('\n').lines().count() == 1,
        "default output must be a single JSON line, got: {stdout}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("default output must be valid JSON");

    assert_eq!(parsed["records"], 4);
    assert_eq!(parsed["nodes"], 4);
    assert_eq!(parsed["edges"], 0);
    assert_eq!(parsed["tombstones"], 0);
    assert_eq!(parsed["diagnostics"], 0);

    // Per-(domain, kind, schema_version) counts.
    let versions = parsed["schema_versions"]
        .as_object()
        .expect("schema_versions must be an object");
    assert_eq!(
        versions[&format!("codegraph:Repository:{SCHEMA_VERSION}")],
        1
    );
    assert_eq!(versions[&format!("codegraph:File:{SCHEMA_VERSION}")], 1);
    assert_eq!(
        versions[&format!("agent_memory:Observation:{AGENT_MEMORY_SCHEMA_VERSION}")],
        1
    );
    assert_eq!(
        versions[&format!("project:Task:{PROJECT_SCHEMA_VERSION}")],
        1
    );

    // Trust-class grouping separates source truth from subjective memory.
    let domains = parsed["domain_counts"]
        .as_object()
        .expect("domain_counts must be an object");
    assert!(domains.contains_key("Deterministic Source Facts"));
    assert!(domains.contains_key("Agent-Authored Claims"));
    assert!(domains.contains_key("Project/Work State"));

    // The embedded source descriptor is part of the documented contract.
    assert_eq!(parsed["source"]["mode"], "embedded");

    // Redaction: raw narrative payloads never appear, only counts and handles.
    assert!(
        !stdout.contains(RAW_PAYLOAD_SENTINEL),
        "inspect output must never include raw record payload text"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_format_text_matches_existing_inspect_style() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = inspect_data_dir_stdout(&data_dir, Some("text"));

    assert!(stdout.contains("records: 4"));
    assert!(stdout.contains("nodes: 4"));
    assert!(stdout.contains("Deterministic Source Facts (codegraph)"));
    assert!(stdout.contains("Agent-Authored Claims (agent_memory)"));
    assert!(stdout.contains("Project/Work State (project)"));
    assert!(!stdout.contains(RAW_PAYLOAD_SENTINEL));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_counts_match_inspect_jsonl_with_full_parity() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, mixed_domain_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let jsonl_output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg(&graph_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let jsonl_parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(jsonl_output).expect("UTF-8"))
            .expect("inspect JSONL output must be valid JSON");

    let store_stdout = inspect_data_dir_stdout(&data_dir, Some("json"));
    let store_parsed: serde_json::Value =
        serde_json::from_str(store_stdout.trim()).expect("inspect store output must be JSON");

    for field in [
        "records",
        "nodes",
        "edges",
        "tombstones",
        "diagnostics",
        "schema_versions",
        "unknown_schema_versions",
        "domain_counts",
        "producer_kinds",
        "egregore_versions",
    ] {
        assert_eq!(
            store_parsed[field], jsonl_parsed[field],
            "field `{field}` must match between --data-dir and JSONL inspection"
        );
    }

    // Repository summaries match after canonical (by-id) ordering.
    let canonical_repos = |value: &serde_json::Value| -> Vec<serde_json::Value> {
        let mut repos = value["repositories"]
            .as_array()
            .expect("repositories must be an array")
            .clone();
        repos.sort_by_key(|repo| repo["id"].as_str().map(str::to_owned));
        repos
    };
    assert_eq!(
        canonical_repos(&store_parsed),
        canonical_repos(&jsonl_parsed)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_is_read_only_and_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");

    // >= 1,000 records spanning three domains and multiple schema-version tuples
    // (issue #125 success metric fixture).
    let mut jsonl = mixed_domain_jsonl();
    for index in 0..400 {
        let file = node_with_version(
            &format!("codegraph:v5:bulk-file-{index}"),
            NodeKind::File,
            SCHEMA_VERSION,
            &format!("src/bulk_{index}.rs"),
            "bulk file record",
        );
        let observation = node_with_version(
            &format!("agent_memory:v1:bulk-obs-{index}"),
            NodeKind::Observation,
            AGENT_MEMORY_SCHEMA_VERSION,
            &format!("bulk-obs-{index}"),
            "bulk observation record",
        );
        let task = node_with_version(
            &format!("project:v1:bulk-task-{index}"),
            NodeKind::Task,
            PROJECT_SCHEMA_VERSION,
            &format!("bulk-task-{index}"),
            "bulk task record",
        );
        for record in [&file, &observation, &task] {
            jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
            jsonl.push('\n');
        }
    }
    fs::write(&graph_path, jsonl).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let before = dir_snapshot(&data_dir);
    let first = inspect_data_dir_stdout(&data_dir, None);
    for run in 1..5 {
        let next = inspect_data_dir_stdout(&data_dir, None);
        assert_eq!(
            first, next,
            "run {run} output must be byte-identical to run 0"
        );
    }
    let after = dir_snapshot(&data_dir);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "inspect must not create or delete any store file"
    );
    assert_eq!(before, after, "inspect must not modify any store file byte");

    let parsed: serde_json::Value =
        serde_json::from_str(first.trim()).expect("output must be valid JSON");
    assert_eq!(parsed["records"], 4 + 3 * 400);
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_labels_unknown_schema_version_distinctly() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");

    let future_version = SCHEMA_VERSION + 1;
    // Write a well-formed node with an unknown future schema version directly
    // into a raw AletheiaDB store (a CLI ingest would reject it).
    {
        let config = aletheiadb::config::durable_config_for_data_dir(&data_dir);
        let db = aletheiadb::AletheiaDB::with_unified_config(config).expect("should open raw db");
        let properties = aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", "codegraph:v6:future-repo")
            .insert("record_type", "node")
            .insert("kind", "Repository")
            .insert("schema_version", i64::from(future_version))
            .insert("domain", "codegraph")
            .build();
        db.create_node("Repository", properties)
            .expect("should create raw node");
    }

    let stdout = inspect_data_dir_stdout(&data_dir, Some("json"));
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output must be valid JSON");

    // Counted and labeled distinctly, never folded into known versions.
    assert_eq!(parsed["records"], 1);
    assert_eq!(
        parsed["unknown_schema_versions"][format!("codegraph:Repository:{future_version}")],
        1
    );
    assert_eq!(
        parsed["schema_versions"]
            .as_object()
            .expect("schema_versions must be an object")
            .len(),
        0
    );
    assert_eq!(parsed["nodes"], 0);
    assert_eq!(parsed["edges"], 0);
    assert_eq!(parsed["tombstones"], 0);
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_missing_store_fails_naming_the_path() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let missing = temp.path().join("does-not-exist");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded store not found"))
        .stderr(predicate::str::contains(missing.to_string_lossy().as_ref()));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn inspect_data_dir_empty_dir_fails_instead_of_reporting_empty_counts() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let empty = temp.path().join("empty-store");
    fs::create_dir_all(&empty).expect("empty dir should be created");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&empty)
        .assert()
        .failure()
        .stderr(predicate::str::contains("is empty"))
        .stderr(predicate::str::contains(empty.to_string_lossy().as_ref()));
}

#[cfg(not(feature = "embedded-aletheiadb"))]
#[test]
fn inspect_data_dir_requires_embedded_feature() {
    let temp = tempfile::tempdir().expect("temp dir should be created");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded-aletheiadb"));
}
