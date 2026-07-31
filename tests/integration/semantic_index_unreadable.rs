#![allow(missing_docs)]
//! Issue #489 — a corrupt vector index must not look like an absent one.
//!
//! `AletheiaDB` 0.2.0 loads per-property vector indexes in parallel WITH ERROR
//! ISOLATION: a corrupted or unreadable index is skipped with a warning instead
//! of aborting the load of every remaining index. Good for a server; for
//! Egregore it turns a loud failure into a quiet one, because a skipped index is
//! simply missing from `list_vector_indexes()` — so "this store was never
//! `--embed`ed" and "this store's index exists and is damaged" arrive at the
//! query gate looking identical. Reporting the second as the first answers a
//! data-loss condition with a benign configuration one.
//!
//! Every store here is built from SYNTHETIC vectors, so no embedding model is
//! ever loaded (no Hugging Face download, no network) — which is also the point:
//! the gate refuses before the query is embedded.

#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::{
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::{
    Graph, GraphRecord, NodeKind, SourceSpan,
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::{
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS, EmbeddingVectorKey, EmbeddingVectorMap,
        default_embedding_model_identity, embedding_index_identity_record,
    },
    stable_id,
};
use assert_cmd::Command;
use serde_json::Value;

/// Builds a store whose vector index is real, persisted, and stamped with the
/// current binary's identity — the state an operator's `--embed` store is in
/// before anything goes wrong.
fn build_embedded_store(data_dir: &Path) {
    let dim = DEFAULT_EMBEDDING_MODEL_DIMENSIONS;
    let mut vectors = EmbeddingVectorMap::new();
    let mut records = Vec::new();
    for i in 0..3_usize {
        let id = stable_id(&["node", "symbol", "src/lib.rs", &format!("sym{i:02}")]);
        let record = GraphRecord::symbol(
            id,
            "function",
            "src/lib.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10 + i,
                start_line: i + 1,
                end_line: i + 1,
            },
            format!("sym{i:02}"),
            format!("fixture symbol number {i}"),
        );
        let mut vector = vec![0.0_f32; dim];
        vector[i % dim] = 1.0;
        vectors.insert(
            EmbeddingVectorKey::from_record(&record).expect("symbol must be embeddable"),
            vector,
        );
        records.push(record);
    }
    records.push(embedding_index_identity_record(
        &default_embedding_model_identity(dim),
    ));

    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dim)
        .expect("fixture store should open");
    for record in &records {
        sink.write_record(record).expect("fixture record writes");
    }
    sink.persist_indexes().expect("fixture indexes persist");
    drop(sink);
}

/// The persisted vector-index directory for the `embedding` property.
///
/// Asserting the path exists also pins the on-disk layout the production probe
/// relies on: if `AletheiaDB` ever moves it, this fails loudly here rather than
/// silently degrading the diagnostic back to "absent".
fn vector_index_dir(data_dir: &Path) -> PathBuf {
    let dir = data_dir
        .join("indexes")
        .join("indexes")
        .join("vector")
        .join("embedding");
    assert!(
        dir.is_dir(),
        "expected a persisted vector index directory at {}",
        dir.display()
    );
    dir
}

/// Corrupts the index the way upstream documents as the skip condition: the
/// metadata file the loader requires is no longer decodable.
fn corrupt_vector_index(data_dir: &Path) -> PathBuf {
    let dir = vector_index_dir(data_dir);
    let meta = dir.join("meta.idx");
    assert!(meta.is_file(), "fixture must persist {}", meta.display());
    fs::write(&meta, b"not a valid meta file").expect("corruption writes");
    dir
}

fn semantic_query(data_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("query")
        .arg("semantic")
        .arg("request timeout handling")
        .arg("--data-dir")
        .arg(data_dir);
    cmd
}

fn stdout_envelope(output: &[u8]) -> Value {
    let text = String::from_utf8_lossy(output);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected a JSON envelope on stdout, got:\n{text}"));
    serde_json::from_str(line).expect("stdout envelope is valid JSON")
}

/// A graph with no embeddable file/symbol nodes, so `--embed` creates the index
/// without ever loading the model.
fn repository_only_graph(path: &Path) {
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "unreadable-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("unreadable-repo".to_owned()),
        "Repository unreadable-repo".to_owned(),
    ));
    fs::write(path, graph.to_jsonl().expect("graph serializes")).expect("graph written");
}

// ── Acceptance: `eg query semantic` must not claim "absent" ──────────────────

/// The core acceptance: a store whose index files exist but failed to load is
/// refused with the distinct `semantic_index_unreadable` code at its own exit
/// code — never the exit-2 "this store was never ingested with embeddings"
/// outcome.
#[test]
fn corrupt_vector_index_is_refused_as_unreadable_not_absent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_embedded_store(&data_dir);
    corrupt_vector_index(&data_dir);

    let assert = semantic_query(&data_dir).assert().code(11);
    let out = assert.get_output();
    let envelope = stdout_envelope(&out.stdout);
    assert_eq!(envelope["ok"], false);
    let error = &envelope["error"];
    assert_eq!(error["code"], "semantic_index_unreadable");
    // The on-disk evidence is disclosed, drawn from a fixed filename table.
    let artifacts = error["index_artifacts"]
        .as_array()
        .expect("index_artifacts is a list");
    assert!(
        artifacts.iter().any(|a| a == "meta.idx"),
        "the refusal must name the index files it found: {artifacts:?}"
    );
    // Which model built the now-unreadable index is still recoverable.
    assert_eq!(
        error["indexed_models"][0]["name"],
        aletheia_egregore::embeddings::DEFAULT_EMBEDDING_MODEL_NAME
    );
    let remedy = error["remedy"].as_str().expect("remedy is a string");
    assert!(
        remedy.contains("FRESH --data-dir"),
        "the remedy must point at a fresh store: {remedy}"
    );
    assert!(
        remedy.contains("overwrites"),
        "the remedy must warn that re-embedding in place overwrites the index files: {remedy}"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("semantic_index_absent"),
        "an index present on disk must never be reported as absent: {stderr}"
    );
    assert!(
        stderr.contains("semantic query refused"),
        "the human summary must say the query was refused: {stderr}"
    );
}

/// Regression guard on the #104 contract: this issue ADDS a case, it does not
/// renumber the existing ones. A store that genuinely was never `--embed`ed
/// still reports `semantic_index_absent` at exit 2.
#[test]
fn store_without_any_index_still_reports_absent_at_exit_two() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    let data_dir = temp.path().join("store");
    repository_only_graph(&graph_path);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let assert = semantic_query(&data_dir).assert().code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("semantic_index_absent"),
        "a never-embedded store keeps the documented absent outcome: {stderr}"
    );
}

/// The refusal is deterministic: repeated runs over an unchanged store are
/// byte-identical on both streams.
#[test]
fn unreadable_refusal_is_byte_identical_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_embedded_store(&data_dir);
    corrupt_vector_index(&data_dir);

    let first = semantic_query(&data_dir).assert().code(11);
    let second = semantic_query(&data_dir).assert().code(11);
    assert_eq!(
        first.get_output().stdout,
        second.get_output().stdout,
        "the refusal envelope must be byte-identical across runs"
    );
    assert_eq!(
        first.get_output().stderr,
        second.get_output().stderr,
        "the human summary must be byte-identical across runs"
    );
}

/// The sibling lanes read the same shared index and carry the same hazard, so
/// they refuse identically.
#[test]
fn semantic_memory_lane_also_refuses_an_unreadable_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_embedded_store(&data_dir);
    corrupt_vector_index(&data_dir);

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("semantic-memory")
        .arg("parser edge case")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .code(11);
    let envelope = stdout_envelope(&assert.get_output().stdout);
    assert_eq!(envelope["error"]["code"], "semantic_index_unreadable");
}

// ── Acceptance: `eg inspect --data-dir` must not claim "absent" ──────────────

/// `eg inspect --data-dir` reports the three-way index status, so an operator
/// can see the store HAS an index that failed to load. `index_dimensions` is
/// null because a skipped index reports none — that is not evidence of absence,
/// and `index_present` stays true.
#[test]
fn inspect_reports_the_index_as_unreadable_not_absent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_embedded_store(&data_dir);
    corrupt_vector_index(&data_dir);

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    let report = stdout_envelope(&assert.get_output().stdout);
    let index = &report["semantic_index"];
    assert_eq!(index["index_status"], "unreadable");
    assert_eq!(index["index_present"], true);
    assert_eq!(index["index_dimensions"], Value::Null);
    assert!(
        index["index_artifacts"]
            .as_array()
            .expect("index_artifacts is a list")
            .iter()
            .any(|a| a == "meta.idx"),
        "the block must disclose the on-disk evidence: {index}"
    );
    // The identity of the model that built the lost index survives.
    assert_eq!(index["identity_recorded"], true);

    let text = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("text")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&text.get_output().stdout);
    assert!(
        stdout.contains("semantic index: PRESENT BUT UNREADABLE"),
        "the text view must not read as absent: {stdout}"
    );
    assert!(
        !stdout.contains("semantic index: absent"),
        "the text view must not claim absence: {stdout}"
    );
}

/// A healthy store is untouched by this change: `loaded`, with its dimensions
/// and identity reported exactly as before.
#[test]
fn healthy_store_still_reports_a_loaded_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    build_embedded_store(&data_dir);

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    let index = &stdout_envelope(&assert.get_output().stdout)["semantic_index"];
    assert_eq!(index["index_status"], "loaded");
    assert_eq!(index["index_present"], true);
    assert_eq!(
        index["index_dimensions"],
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS
    );
    assert_eq!(index["index_artifacts"], Value::Null);
}

// ── The overwrite footgun ───────────────────────────────────────────────────

/// Upstream documents that enabling a vector index over skipped index files
/// creates an EMPTY index whose next persistence cycle overwrites them,
/// permanently losing the indexed vectors. `--embed` into such a store must
/// therefore refuse BEFORE enabling anything — and leave the files untouched,
/// so the store stays repairable.
#[test]
fn embedding_into_an_unreadable_store_is_refused_and_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    build_embedded_store(&data_dir);
    let index_dir = corrupt_vector_index(&data_dir);
    repository_only_graph(&graph_path);

    let before: Vec<(PathBuf, Vec<u8>)> = index_snapshot(&index_dir);

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("semantic_index_unreadable"),
        "the refusal must name the unreadable index: {stderr}"
    );
    assert!(
        stderr.contains("overwrite"),
        "the refusal must say why it refuses rather than proceeding: {stderr}"
    );

    assert_eq!(
        before,
        index_snapshot(&index_dir),
        "a refused --embed must leave the persisted index files byte-identical"
    );
}

/// Byte snapshot of every file in a persisted index directory, ordered by path.
fn index_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = fs::read_dir(dir)
        .expect("index directory reads")
        .map(|entry| {
            let path = entry.expect("directory entry").path();
            let bytes = fs::read(&path).unwrap_or_default();
            (path, bytes)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}
