#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use predicates::prelude::*;
use serde::Deserialize;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[derive(Debug, Deserialize)]
struct DaemonMetadata {
    pid: u32,
    address: String,
    token: String,
}

#[test]
fn foreground_daemon_status_stop_and_requires_auth() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");

    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("status")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("daemon running"))
        .stderr(predicate::str::is_empty());

    let metadata = read_metadata(&data_dir);
    let response = http_request(
        &metadata.address,
        "GET /v1/status HTTP/1.1\r\nHost: egregore\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "status endpoint should require auth, got {response}"
    );

    daemon.stop();
}

#[test]
fn second_daemon_for_same_data_dir_fails() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--port")
        .arg("0")
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon already running"));

    daemon.stop();
}

#[test]
fn daemon_stop_cleans_stale_metadata() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let address = listener
        .local_addr()
        .expect("ephemeral address should exist")
        .to_string();
    drop(listener);
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_999,
            "address": address,
            "token": "stale-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("stale metadata should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("stop")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .success()
        .stdout(predicate::str::contains("daemon stopped"));

    assert!(
        !metadata_path.exists(),
        "stale daemon metadata should be removed"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_cli_ingest_refuses_while_daemon_owns_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded store is already leased"));

    daemon.stop();
}

#[test]
fn embedded_cli_ingest_refuses_when_daemon_lock_exists_without_metadata() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    let metadata_path = runtime_dir(&data_dir).join("egregored.json");
    let metadata = fs::read_to_string(&metadata_path).expect("metadata should be readable");
    fs::remove_file(&metadata_path).expect("metadata should be removable");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("embedded store is already leased"));

    fs::write(metadata_path, metadata).expect("metadata should be restored for shutdown");
    daemon.stop();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn daemon_ingest_reads_back_records_and_deduplicates_retries() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let stdout = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--agent-id")
        .arg("test-agent")
        .arg("--session-id")
        .arg("test-session")
        .arg("--idempotency-key")
        .arg("fixture-ingest")
        .assert()
        .success()
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("stdout should be utf8");
    assert!(stdout.contains("idempotent: false"));

    let retry_stdout = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--agent-id")
        .arg("test-agent")
        .arg("--session-id")
        .arg("test-session")
        .arg("--idempotency-key")
        .arg("fixture-ingest")
        .assert()
        .success()
        .stdout(predicate::str::contains("idempotent: true"))
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let retry_stdout = String::from_utf8(retry_stdout).expect("stdout should be utf8");
    assert!(retry_stdout.contains("failed: 0"));

    let first_record_id = first_record_id(&graph_path);
    let metadata = read_metadata(&data_dir);
    let response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/{first_record_id} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "record read-back should succeed, got {response}"
    );
    assert!(response.contains(&first_record_id));

    daemon.stop();
}

#[test]
fn daemon_recovers_pending_idempotency_receipt_after_restart() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("restart-recovery")
        .assert()
        .success()
        .stdout(predicate::str::contains("idempotent: false"));

    daemon.stop();

    let idempotency_path = runtime_dir(&data_dir).join("idempotency.json");
    let mut idempotency_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&idempotency_path).expect("idempotency file should be readable"),
    )
    .expect("idempotency file should parse");
    let entry = &mut idempotency_json["entries"]["restart-recovery"];
    let payload_hash = entry["payload_hash"]
        .as_str()
        .expect("entry should include payload hash")
        .to_owned();
    let record_ids = entry["response"]["record_ids"].clone();
    *entry = serde_json::json!({
        "state": "pending",
        "payload_hash": payload_hash,
        "record_ids": record_ids,
    });
    fs::write(
        &idempotency_path,
        serde_json::to_vec_pretty(&idempotency_json).expect("idempotency JSON should serialize"),
    )
    .expect("pending idempotency file should write");

    let mut restarted = start_daemon(&data_dir);
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("restart-recovery")
        .assert()
        .success()
        .stdout(predicate::str::contains("idempotent: true"));

    let recovered_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&idempotency_path).expect("idempotency file should be readable"),
    )
    .expect("idempotency file should parse");
    assert_eq!(
        recovered_json["entries"]["restart-recovery"]["state"],
        "committed"
    );

    restarted.stop();
}

#[test]
fn daemon_does_not_commit_when_idempotency_receipt_reservation_fails() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let metadata = read_metadata(&data_dir);
    let idempotency_path = runtime_dir(&data_dir).join("idempotency.json");
    let blocked_tmp_path = idempotency_path.with_extension(format!("tmp.{}", metadata.pid));
    fs::create_dir(&blocked_tmp_path).expect("idempotency temp path should be blocked");
    let first_record_id = first_record_id(&graph_path);
    let records = graph_records_json(&graph_path);
    let ingest_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "locked-idempotency",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "locked-idempotency",
            "domain": "codegraph",
            "created_at": "2026-05-17T00:00:00Z",
            "payload": { "records": records }
        }),
    );
    assert!(
        ingest_response.starts_with("HTTP/1.1 500"),
        "blocked idempotency receipt should fail before commit, got {ingest_response}"
    );
    fs::remove_dir(&blocked_tmp_path).expect("blocked temp path should be removable");

    let read_response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/{first_record_id} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        read_response.contains("\"record\":null"),
        "record should not commit when receipt reservation fails, got {read_response}"
    );

    daemon.stop();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn daemon_registers_agents_runs_ingest_jobs_and_queries_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let metadata = read_metadata(&data_dir);
    let register_response = http_json(
        &metadata,
        "POST",
        "/v1/agents/register",
        &serde_json::json!({
            "request_id": "register-1",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "agent_kind": "codex",
            "project_scope": "egregore"
        }),
    );
    assert!(
        register_response.starts_with("HTTP/1.1 200"),
        "agent registration should succeed, got {register_response}"
    );
    assert!(register_response.contains("AgentSession"));

    let records = graph_records_json(&graph_path);
    let job_response = http_json(
        &metadata,
        "POST",
        "/v1/jobs/ingest",
        &serde_json::json!({
            "request_id": "job-1",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "job-fixture-ingest",
            "domain": "codegraph",
            "created_at": "2026-05-17T00:00:00Z",
            "payload": { "records": records }
        }),
    );
    assert!(
        job_response.starts_with("HTTP/1.1 202"),
        "job ingest should be accepted, got {job_response}"
    );
    let job_id = response_json(&job_response)["job_id"]
        .as_str()
        .expect("job response should include id")
        .to_owned();

    let job_status = wait_for_job(&metadata, &job_id);
    assert_eq!(job_status["status"], "completed");
    assert_eq!(job_status["report"]["failed"], 0);

    let first_record_id = first_record_id(&graph_path);
    let query_response = http_json(
        &metadata,
        "POST",
        "/v1/query",
        &serde_json::json!({
            "request_id": "query-1",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "domain": "codegraph",
            "limit": 1,
            "record_ids": [first_record_id]
        }),
    );
    assert!(
        query_response.starts_with("HTTP/1.1 200"),
        "query should succeed, got {query_response}"
    );
    assert!(query_response.contains(&first_record_id));

    daemon.stop();
}

struct RunningDaemon {
    child: Option<Child>,
    data_dir: PathBuf,
}

impl RunningDaemon {
    fn stop(&mut self) {
        stop_daemon(&self.data_dir);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
                .arg("daemon")
                .arg("stop")
                .arg("--data-dir")
                .arg(&self.data_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start_daemon(data_dir: &Path) -> RunningDaemon {
    let child = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon should spawn");
    let _ = read_metadata(data_dir);
    RunningDaemon {
        child: Some(child),
        data_dir: data_dir.to_path_buf(),
    }
}

fn stop_daemon(data_dir: &Path) {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("stop")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

fn read_metadata(data_dir: &Path) -> DaemonMetadata {
    let metadata_path = runtime_dir(data_dir).join("egregored.json");
    let start = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(&metadata_path)
            && let Ok(metadata) = serde_json::from_str(&contents)
        {
            return metadata;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "daemon metadata should appear at {}",
            metadata_path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn runtime_dir(data_dir: &Path) -> PathBuf {
    data_dir.file_name().map_or_else(
        || data_dir.join(".egregore-runtime"),
        |file_name| {
            let mut runtime_name = file_name.to_os_string();
            runtime_name.push(".egregore-runtime");
            data_dir.with_file_name(runtime_name)
        },
    )
}

fn http_json(
    metadata: &DaemonMetadata,
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> String {
    let body = serde_json::to_string(body).expect("body should serialize");
    http_request(
        &metadata.address,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            metadata.token,
            body.len()
        ),
    )
}

fn http_request(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("daemon should accept connections");
    stream
        .write_all(request.as_bytes())
        .expect("request should write");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response should read");
    response
}

fn graph_records_json(graph_path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(graph_path)
        .expect("graph should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse as JSON"))
        .collect()
}

fn wait_for_job(metadata: &DaemonMetadata, job_id: &str) -> serde_json::Value {
    let start = Instant::now();
    loop {
        let response = http_request(
            &metadata.address,
            &format!(
                "GET /v1/jobs/{job_id} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
                metadata.token
            ),
        );
        if response.starts_with("HTTP/1.1 200") {
            let body = response_json(&response);
            if body["status"] == "completed" || body["status"] == "failed" {
                return body;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "job should finish, last response: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn response_json(response: &str) -> serde_json::Value {
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .expect("response should contain body");
    serde_json::from_str(body).expect("response body should be JSON")
}

fn first_record_id(graph_path: &Path) -> String {
    let jsonl = fs::read_to_string(graph_path).expect("graph should be readable");
    let value: serde_json::Value =
        serde_json::from_str(jsonl.lines().next().expect("graph should have records"))
            .expect("record should parse as JSON");
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("record should have id")
        .to_owned()
}
