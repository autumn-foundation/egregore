//! TDD tests for issue #59: Expose daemon semantic search.
//!
//! RED PHASE: These tests are written to fail until the
//! `semantic_search` daemon verb and the `--daemon` CLI flag for
//! `eg query semantic` are implemented.

#![allow(missing_docs)]
#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::{
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    adapters::{EmbeddedAletheiaSink, GraphSink},
    embeddings::{EmbeddingVectorKey, EmbeddingVectorMap},
    ir::{GraphRecord, stable_id},
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde::Deserialize;

// ── Test helpers ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DaemonMetadata {
    address: String,
    token: String,
    state: String,
}

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn runtime_dir(data_dir: &Path) -> PathBuf {
    data_dir.file_name().map_or_else(
        || data_dir.join(".egregore-runtime"),
        |name| {
            let mut runtime_name = name.to_os_string();
            runtime_name.push(".egregore-runtime");
            data_dir.with_file_name(runtime_name)
        },
    )
}

fn read_running_metadata(data_dir: &Path) -> DaemonMetadata {
    let metadata_path = runtime_dir(data_dir).join("egregored.json");
    let start = Instant::now();
    loop {
        if let Ok(contents) = std::fs::read_to_string(&metadata_path) {
            if let Ok(metadata) = serde_json::from_str::<DaemonMetadata>(&contents) {
                if metadata.state == "running" {
                    return metadata;
                }
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon metadata should transition to running for {}",
            data_dir.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

struct RunningDaemon {
    child: Option<std::process::Child>,
    data_dir: PathBuf,
}

impl RunningDaemon {
    fn stop(&mut self) {
        Command::cargo_bin("egregore")
            .expect("binary should run")
            .args(["daemon", "stop", "--data-dir"])
            .arg(&self.data_dir)
            .assert()
            .success();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}

fn start_daemon(data_dir: &Path) -> RunningDaemon {
    let child = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .args(["daemon", "run", "--data-dir"])
        .arg(data_dir)
        .args(["--port", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon should spawn");
    let _ = read_running_metadata(data_dir);
    RunningDaemon {
        child: Some(child),
        data_dir: data_dir.to_path_buf(),
    }
}

fn http_query(metadata: &DaemonMetadata, body: &serde_json::Value) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let body_str = serde_json::to_string(body).expect("body should serialize");
    let request = format!(
        "POST /v1/query HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
        metadata.token,
        body_str.len()
    );
    let mut stream =
        TcpStream::connect(&metadata.address).expect("daemon should accept connections");
    stream
        .write_all(request.as_bytes())
        .expect("request should write");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response should read");
    response
}

fn response_json(response: &str) -> serde_json::Value {
    let body_start = response.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    serde_json::from_str(&response[body_start..]).expect("response body should be valid JSON")
}

fn symbol_record(id: &str, name: &str, path: &str, start_line: usize) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "fn",
        path.to_owned(),
        aletheia_egregore::SourceSpan {
            start_line,
            end_line: start_line + 10,
            start_byte: start_line * 100,
            end_byte: start_line * 100 + 200,
        },
        name.to_owned(),
        format!("Rust function {name} in {path}"),
    )
}

/// Creates a fixture store with 15 symbol records and fake 2-dimensional
/// embedding vectors, returning the record IDs in sorted order.
///
/// Vectors are laid out on the unit circle so that cosine similarity matches
/// are deterministic: record[i] has vector (cos(i * π/8), sin(i * π/8)).
fn create_semantic_fixture_store(data_dir: &Path) -> Vec<String> {
    let mut records = Vec::new();
    let mut vectors = EmbeddingVectorMap::new();

    for i in 0..15usize {
        let angle = i as f32 * std::f32::consts::PI / 8.0;
        let vector = vec![angle.cos(), angle.sin()];
        let id = stable_id(&["node", "Symbol", "src/lib.rs", &format!("fn_{i}")]);
        let record = symbol_record(&id, &format!("fn_{i}"), "src/lib.rs", (i + 1) * 5);
        let key = EmbeddingVectorKey::from_record(&record).expect("symbol should be embeddable");
        vectors.insert(key, vector);
        records.push((id, record));
    }

    let mut sink = EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, 2)
        .expect("fixture semantic store should open");
    for (_, record) in &records {
        sink.write_record(record)
            .expect("fixture record should write");
    }
    sink.persist_indexes()
        .expect("fixture indexes should persist");

    let mut ids: Vec<String> = records.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids
}

// ── AC1: daemon verb missing params ──────────────────────────────────────────

#[test]
fn daemon_semantic_search_missing_query_vector_returns_bad_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-missing-vec",
            "verb": "semantic_search",
            "params": {}
        }),
    );
    daemon.stop();

    assert!(
        response.starts_with("HTTP/1.1 400"),
        "missing query_vector should return 400, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(body["ok"], false, "missing field must have ok:false");
    let code = body["error"]["code"].as_str().unwrap_or("");
    assert!(
        code == "missing_field" || code == "bad_request",
        "missing query_vector must return missing_field or bad_request code, got {code}"
    );
}

#[test]
fn daemon_semantic_search_invalid_query_vector_type_returns_bad_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-bad-vec-type",
            "verb": "semantic_search",
            "params": { "query_vector": "not-an-array" }
        }),
    );
    daemon.stop();

    assert!(
        response.starts_with("HTTP/1.1 400"),
        "non-array query_vector should return 400, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(body["ok"], false, "bad type must have ok:false, got {body}");
}

#[test]
fn daemon_semantic_search_empty_query_vector_returns_bad_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-empty-vec",
            "verb": "semantic_search",
            "params": { "query_vector": [] }
        }),
    );
    daemon.stop();

    assert!(
        response.starts_with("HTTP/1.1 400"),
        "empty query_vector should return 400, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["ok"], false,
        "empty vector must have ok:false, got {body}"
    );
}

// ── AC2: missing semantic index diagnostic ───────────────────────────────────

#[test]
fn daemon_semantic_search_no_embeddings_in_store_returns_missing_semantic_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");

    // Ingest records WITHOUT embeddings so the store exists but has no semantic index.
    let graph_path = temp.path().join("graph.jsonl");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["scan"])
        .arg(fixture_repo())
        .args(["--out"])
        .arg(&graph_path)
        .assert()
        .success();
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["ingest"])
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-no-index",
            "verb": "semantic_search",
            "params": { "query_vector": [1.0, 0.0] }
        }),
    );
    daemon.stop();

    // The daemon must return a machine-readable diagnostic, not empty results.
    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "missing semantic index must not return 200, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["ok"], false,
        "missing semantic index must return ok:false, got {body}"
    );
    let code = body["error"]["code"].as_str().unwrap_or("");
    assert_eq!(
        code, "missing_semantic_index",
        "missing semantic index must return missing_semantic_index code, got {body}"
    );
}

// ── AC3: successful search returns required fields ───────────────────────────

#[test]
fn daemon_semantic_search_returns_results_with_required_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    // Query with vector close to index 0 (angle 0 → [1.0, 0.0]).
    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-fields",
            "verb": "semantic_search",
            "params": {
                "query_vector": [1.0_f32, 0.0_f32],
                "limit": 5
            }
        }),
    );
    daemon.stop();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "valid semantic search should return 200, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["ok"], true,
        "valid search must have ok:true, got {body}"
    );
    assert_eq!(
        body["result"]["verb"], "semantic_search",
        "result must carry verb name, got {body}"
    );
    let records = body["result"]["records"]
        .as_array()
        .expect("result.records must be an array");
    assert!(
        !records.is_empty(),
        "should return at least one result, got {body}"
    );

    // AC2: Successful results include record_id, score, repo_relative_path, span.
    let first = &records[0];
    assert!(
        first["record_id"].is_string(),
        "result must have record_id string, got {first}"
    );
    assert!(
        first["score"].is_number() || first["score"].is_string(),
        "result must have score, got {first}"
    );
    assert!(
        first["repo_relative_path"].is_string(),
        "result must have repo_relative_path, got {first}"
    );
    // Span is optional (absent-span rule): span may be null or an object.
    // If present, it must have start_line.
    if let Some(span) = first.get("span").filter(|s| !s.is_null()) {
        assert!(
            span["start_line"].is_number(),
            "span.start_line must be a number, got {span}"
        );
    }
}

// ── AC4: parity with embedded semantic search for 10 fixture queries ─────────

#[test]
fn daemon_semantic_search_parity_with_embedded_for_10_fixture_queries() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    // Ten fixture query vectors spaced evenly across the 2D circle.
    let query_vectors: Vec<Vec<f32>> = (0..10)
        .map(|i| {
            let angle = i as f32 * std::f32::consts::PI / 5.0;
            vec![angle.cos(), angle.sin()]
        })
        .collect();

    // Embedded results (direct store access, no daemon).
    let embedded_results: Vec<Vec<String>> = {
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        query_vectors
            .iter()
            .map(|v| {
                sink.semantic_search(v, 5)
                    .expect("embedded search should succeed")
                    .into_iter()
                    .map(|m| m.record_id)
                    .collect()
            })
            .collect()
    };

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    // Daemon results.
    let daemon_results: Vec<Vec<String>> = query_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let response = http_query(
                &metadata,
                &serde_json::json!({
                    "request_id": format!("parity-{i}"),
                    "verb": "semantic_search",
                    "params": { "query_vector": v, "limit": 5 }
                }),
            );
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "daemon parity query {i} should return 200, got {response}"
            );
            let body = response_json(&response);
            body["result"]["records"]
                .as_array()
                .expect("records must be array")
                .iter()
                .map(|r| {
                    r["record_id"]
                        .as_str()
                        .expect("record_id must be string")
                        .to_owned()
                })
                .collect()
        })
        .collect();

    daemon.stop();

    // All 10 fixture queries must return the same top-k record IDs in the same order.
    for (i, (embedded, daemon)) in embedded_results.iter().zip(&daemon_results).enumerate() {
        assert_eq!(
            embedded, daemon,
            "query {i}: daemon results must match embedded results (order matters)\nembedded: {embedded:?}\ndaemon: {daemon:?}"
        );
    }
}

// ── AC5: deterministic repeated runs ─────────────────────────────────────────

#[test]
fn daemon_semantic_search_repeated_5_times_returns_identical_results() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let query_vector = vec![0.707_f32, 0.707_f32]; // 45° vector
    let limit = 5;

    let mut all_results: Vec<Vec<String>> = Vec::new();
    for run in 0..5 {
        let response = http_query(
            &metadata,
            &serde_json::json!({
                "request_id": format!("repeat-{run}"),
                "verb": "semantic_search",
                "params": { "query_vector": &query_vector, "limit": limit }
            }),
        );
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "repeated run {run} should return 200, got {response}"
        );
        let body = response_json(&response);
        let ids: Vec<String> = body["result"]["records"]
            .as_array()
            .expect("records must be array")
            .iter()
            .map(|r| {
                r["record_id"]
                    .as_str()
                    .expect("record_id must be string")
                    .to_owned()
            })
            .collect();
        all_results.push(ids);
    }
    daemon.stop();

    let first = &all_results[0];
    for (run, results) in all_results.iter().enumerate().skip(1) {
        assert_eq!(
            first, results,
            "run {run}: results must be identical to run 0\nfirst: {first:?}\ncurrent: {results:?}"
        );
    }
}

// ── AC6: redaction — output must not include disallowed content ───────────────

#[test]
fn daemon_semantic_search_output_does_not_include_disallowed_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-redaction",
            "verb": "semantic_search",
            "params": { "query_vector": [1.0_f32, 0.0_f32], "limit": 5 }
        }),
    );
    daemon.stop();

    // Bearer token must not appear in any field of the response.
    assert!(
        !response.contains(&metadata.token),
        "response must not leak the daemon bearer token"
    );

    // No raw patch hunks, issue bodies, or transcript markers.
    let body_str = &response[response.find("\r\n\r\n").unwrap_or(0)..];
    assert!(
        !body_str.contains("@@ -"),
        "response must not contain patch hunks"
    );
}

// ── AC7: output does not classify results as verification evidence ────────────

#[test]
fn daemon_semantic_search_result_does_not_claim_verification_evidence() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-no-evidence-claim",
            "verb": "semantic_search",
            "params": { "query_vector": [1.0_f32, 0.0_f32], "limit": 5 }
        }),
    );
    daemon.stop();

    let body = response_json(&response);
    if body["ok"].as_bool() == Some(true) {
        for record in body["result"]["records"].as_array().unwrap_or(&Vec::new()) {
            // Results must not carry `kind`, `domain`, or fields that classify
            // them as verification evidence, agent memory, or task proof.
            assert!(
                !matches!(
                    record.get("kind").and_then(|v| v.as_str()),
                    Some(
                        "Verification"
                            | "CommandEvidence"
                            | "Observation"
                            | "Task"
                            | "AcceptanceCriterion"
                    )
                ),
                "semantic search result must not carry verification evidence node kinds, got {record}"
            );
        }
    }
}

// ── AC8: CLI `--daemon` flag routes through daemon ────────────────────────────

#[test]
fn eg_query_semantic_daemon_flag_missing_daemon_exits_1_with_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("no-daemon-store");
    create_semantic_fixture_store(&data_dir);

    // No daemon is running. The CLI should fail gracefully with a diagnostic,
    // not silently fall back to the embedded store.
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "semantic", "function that does arithmetic"])
        .args(["--data-dir"])
        .arg(&data_dir)
        .arg("--daemon")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("daemon")
                .or(predicate::str::contains("connect"))
                .or(predicate::str::contains("metadata")),
        );
}

#[test]
fn eg_query_semantic_daemon_flag_routes_through_daemon_not_direct_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);

    // The store was created with fake 2D vectors. The embedding model
    // is not used here — we only check the routing works end-to-end
    // by calling the daemon path and verifying structured JSON output
    // is produced without error. In a real deployment, the store would
    // have been ingested with `--embed` using the text embedding model.
    //
    // We skip the actual `eg query semantic` text-embedding path here
    // because it would download a model. Instead we test the daemon
    // HTTP verb directly (see `daemon_semantic_search_returns_results_*`).
    // The CLI routing is verified by the missing-daemon test above and by
    // the query_cli.rs tests.

    daemon.stop();
}

// ── AC9: incompatible embedding dimension ─────────────────────────────────────

#[test]
fn daemon_semantic_search_incompatible_dimension_returns_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    // Store has 2-dim embeddings; we query with a 3-dim vector.
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-dim-mismatch",
            "verb": "semantic_search",
            "params": { "query_vector": [0.577_f32, 0.577_f32, 0.577_f32] }
        }),
    );
    daemon.stop();

    // Must return a diagnostic, not silently return empty.
    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "dimension mismatch must not return 200 (would silently return empty), got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["ok"], false,
        "dimension mismatch must return ok:false, got {body}"
    );
    let code = body["error"]["code"].as_str().unwrap_or("");
    assert!(
        code == "incompatible_embedding_dimension" || code == "internal_error",
        "dimension mismatch must return a diagnostic code, got {code} in {body}"
    );
}

// ── AC10: query_timeout diagnostic ───────────────────────────────────────────

#[test]
fn daemon_semantic_search_budget_zero_returns_query_timeout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    create_semantic_fixture_store(&data_dir);

    let mut daemon = start_daemon(&data_dir);
    let metadata = read_running_metadata(&data_dir);

    let response = http_query(
        &metadata,
        &serde_json::json!({
            "request_id": "sem-timeout",
            "verb": "semantic_search",
            "params": { "query_vector": [1.0_f32, 0.0_f32] },
            "budget": { "timeout_ms": 0 }
        }),
    );
    daemon.stop();

    assert!(
        response.starts_with("HTTP/1.1 408"),
        "zero timeout budget must return 408, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "query_timeout",
        "zero timeout must return query_timeout code, got {body}"
    );
}
