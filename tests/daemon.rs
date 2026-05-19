#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    adapters::EmbeddedAletheiaSink,
    daemon::{DaemonClient, DaemonMetadata as ClientDaemonMetadata, StoreLease},
    ir::{EdgeLabel, GraphRecord, NodeKind, TemporalMetadata},
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
fn daemon_status_rejects_copied_metadata_for_another_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let first_data_dir = temp.path().join("first-store");
    let second_data_dir = temp.path().join("second-store");
    let mut daemon = start_daemon(&second_data_dir);
    let second_metadata_path = runtime_dir(&second_data_dir).join("egregored.json");
    let first_metadata_path = runtime_dir(&first_data_dir).join("egregored.json");
    let second_metadata = fs::read_to_string(&second_metadata_path)
        .expect("second daemon metadata should be readable");
    fs::create_dir_all(
        first_metadata_path
            .parent()
            .expect("first metadata should have a parent"),
    )
    .expect("first runtime dir should be created");
    fs::write(&first_metadata_path, second_metadata)
        .expect("copied daemon metadata should be written");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("status")
        .arg("--data-dir")
        .arg(&first_data_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));

    daemon.stop();
}

#[test]
fn daemon_stop_does_not_wait_for_slow_request_headers() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let mut stream =
        TcpStream::connect(&metadata.address).expect("slow request connection should open");
    stream
        .write_all(b"POST /v1/status HTTP/1.1\r\nHost: egregore\r\n")
        .expect("partial request should write");
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    daemon.stop();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "daemon shutdown should not wait for a trickling request"
    );
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
fn store_lease_uses_canonical_data_dir_identity() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let alias_dir = temp.path().join("store-alias");
    fs::create_dir_all(&data_dir).expect("store dir should be created");
    if let Err(error) = create_dir_symlink(&data_dir, &alias_dir) {
        eprintln!(
            "skipping canonical lease alias assertion because directory symlinks are unavailable: {error}"
        );
        return;
    }

    let _lease = StoreLease::acquire(&data_dir).expect("primary lease should acquire");
    let alias_attempt = StoreLease::acquire(&alias_dir);
    assert!(
        alias_attempt.is_err(),
        "alias path should contend for the same physical store lease"
    );
}

#[test]
fn public_embedded_sink_respects_store_lease() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let _lease = StoreLease::acquire(&data_dir).expect("test should hold store lease");

    let result = EmbeddedAletheiaSink::open(&data_dir);

    assert!(
        result.is_err(),
        "public embedded sink open should not bypass an active store lease"
    );
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

#[test]
fn daemon_status_times_out_stalled_stale_metadata() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let address = listener
        .local_addr()
        .expect("ephemeral address should exist")
        .to_string();
    let listener_thread = thread::spawn(move || {
        let Ok((_stream, _)) = listener.accept() else {
            return;
        };
        thread::sleep(Duration::from_secs(5));
    });
    fs::write(
        runtime_dir.join("egregored.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_998,
            "address": address,
            "token": "stalled-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("stalled metadata should write");

    let start = Instant::now();
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("status")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "stalled metadata probe should time out promptly"
    );
    listener_thread
        .join()
        .expect("listener thread should finish");
}

#[test]
fn daemon_status_rejects_wrong_service_health_response() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let address = listener
        .local_addr()
        .expect("ephemeral address should exist")
        .to_string();
    let listener_thread = thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
    });
    fs::write(
        runtime_dir.join("egregored.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_997,
            "address": address,
            "token": "wrong-service-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("wrong-service metadata should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("status")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));
    listener_thread
        .join()
        .expect("listener thread should finish");
}

#[test]
fn daemon_status_rejects_same_version_wrong_store_health_response() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let address = listener
        .local_addr()
        .expect("ephemeral address should exist")
        .to_string();
    let wrong_data_dir = temp.path().join("other-store");
    let response_body = serde_json::to_vec(&serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": wrong_data_dir
    }))
    .expect("health body should serialize");
    let listener_thread = thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            String::from_utf8(response_body).expect("health body should be utf8")
        );
        let _ = stream.write_all(response.as_bytes());
    });
    fs::write(
        runtime_dir.join("egregored.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_993,
            "address": address,
            "token": "wrong-store-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("wrong-store metadata should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("status")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("daemon not running"));
    listener_thread
        .join()
        .expect("listener thread should finish");
}

#[test]
fn daemon_ingest_preflights_wrong_service_before_sending_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    write_graph(
        &graph_path,
        &[GraphRecord::node(
            "codegraph:v1:wrong-service-ingest-node".to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "should not be sent".to_owned(),
        )],
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port should bind");
    let address = listener
        .local_addr()
        .expect("ephemeral address should exist")
        .to_string();
    let listener_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("health request should arrive");
        let mut request = [0_u8; 4096];
        let read = stream
            .read(&mut request)
            .expect("health request should read");
        let request = String::from_utf8_lossy(&request[..read]).to_string();
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        request
    });
    fs::write(
        runtime_dir.join("egregored.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_994,
            "address": address,
            "token": "wrong-service-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("wrong-service metadata should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .arg("--idempotency-key")
        .arg("wrong-service-ingest")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "daemon health validation failed before ingest",
        ));

    let request = listener_thread
        .join()
        .expect("listener thread should finish");
    assert!(
        request.starts_with("GET /v1/health "),
        "mutating daemon client should preflight health before POST, got {request}"
    );
    assert!(
        !request.contains("wrong-service-token") && !request.contains("record_type"),
        "health preflight should not send daemon token or graph records, got {request}"
    );
}

#[test]
fn daemon_health_probe_bounds_unroutable_connect() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let client = DaemonClient::new(ClientDaemonMetadata {
        pid: 999_995,
        address: "10.255.255.1:9".to_owned(),
        token: "blackhole-token".to_owned(),
        data_dir: temp.path().join("store"),
        version: "test".to_owned(),
        started_at_unix_ms: 0,
    });

    let started = Instant::now();
    let result = client.health();
    assert!(result.is_err(), "blackhole probe should fail");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "blackhole probe should be bounded by the daemon client timeout"
    );
}

#[test]
fn daemon_stop_preserves_unresponsive_metadata_when_store_lease_is_held() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let _lease = StoreLease::acquire(&data_dir).expect("test should hold store lease");
    let metadata_path = runtime_dir.join("egregored.json");
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": 999_996,
            "address": "127.0.0.1:9",
            "token": "held-lease-token",
            "data_dir": data_dir,
            "version": "test",
            "started_at_unix_ms": 0_u64
        }))
        .expect("metadata should serialize"),
    )
    .expect("metadata should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("daemon")
        .arg("stop")
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("store lease is still held"));

    assert!(
        metadata_path.exists(),
        "unresponsive owner metadata should not be deleted while the lease is held"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_cli_ingest_refuses_while_daemon_owns_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    let record = GraphRecord::node(
        "codegraph:v1:restart-recovery-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "restart recovery".to_owned(),
    );
    fs::write(
        &graph_path,
        serde_json::to_string(&record).expect("record should serialize") + "\n",
    )
    .expect("restart recovery graph should write");

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
    let record = GraphRecord::node(
        "codegraph:v1:restart-recovery-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "restart recovery".to_owned(),
    );
    fs::write(
        &graph_path,
        serde_json::to_string(&record).expect("record should serialize") + "\n",
    )
    .expect("restart recovery graph should write");

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

    let record = GraphRecord::node(
        "codegraph:v1:restart-recovery-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "restart recovery".to_owned(),
    );
    fs::write(
        &graph_path,
        serde_json::to_string(&record).expect("record should serialize") + "\n",
    )
    .expect("restart recovery graph should write");

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
    let restart_recovery_key = cli_scoped_idempotency_key("restart-recovery");
    let entry = &mut idempotency_json["entries"][restart_recovery_key.as_str()];
    let payload_hash = entry["payload_hash"]
        .as_str()
        .expect("entry should include payload hash")
        .to_owned();
    let record_ids = entry["response"]["record_ids"].clone();
    *entry = serde_json::json!({
        "state": "pending",
        "payload_hash": payload_hash,
        "record_ids": record_ids,
        "records": graph_records_json(&graph_path),
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
        recovered_json["entries"][restart_recovery_key.as_str()]["state"],
        "committed"
    );

    restarted.stop();
}

#[test]
fn daemon_pending_recovery_rejects_same_id_mismatch() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);

    let first = GraphRecord::node(
        "codegraph:v1:same-id-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "old".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v1:same-id-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "new".to_owned(),
    );

    let metadata = read_metadata(&data_dir);
    let first_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "same-id-first",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "same-id-first",
            "domain": "codegraph",
            "created_at": "2026-05-17T00:00:00Z",
            "payload": { "records": [first] }
        }),
    );
    assert!(
        first_response.starts_with("HTTP/1.1 200"),
        "first same-id ingest should succeed, got {first_response}"
    );
    daemon.stop();

    let second_hash = blake3::hash(
        &serde_json::to_vec(&vec![second.clone()]).expect("record JSON should serialize"),
    )
    .to_hex()
    .to_string();
    let idempotency_path = runtime_dir(&data_dir).join("idempotency.json");
    let mut idempotency_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&idempotency_path).expect("idempotency file should be readable"),
    )
    .expect("idempotency file should parse");
    let same_id_second_key = cli_scoped_idempotency_key("same-id-second");
    idempotency_json["entries"][same_id_second_key.as_str()] = serde_json::json!({
        "state": "pending",
        "payload_hash": second_hash,
        "record_ids": ["codegraph:v1:same-id-node"],
        "records": [second],
    });
    fs::write(
        &idempotency_path,
        serde_json::to_vec_pretty(&idempotency_json).expect("idempotency JSON should serialize"),
    )
    .expect("pending same-id idempotency file should write");

    let mut restarted = start_daemon(&data_dir);
    let graph_path = temp.path().join("same-id-update.jsonl");
    fs::write(&graph_path, serde_json::to_string(&second).unwrap() + "\n")
        .expect("same-id update graph should write");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("same-id-second")
        .assert()
        .failure()
        .stderr(predicate::str::contains("conflicting committed records"));

    let metadata = read_metadata(&data_dir);
    let read_response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/codegraph:v1:same-id-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        read_response.contains("\"summary\":\"old\""),
        "ambiguous same-id pending retry should leave existing record current, got {read_response}"
    );

    restarted.stop();
}

#[test]
fn daemon_pending_recovery_rejects_stale_same_id_replay() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let old = GraphRecord::node(
        "codegraph:v1:stale-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "old".to_owned(),
    );
    let new = GraphRecord::node(
        "codegraph:v1:stale-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "new".to_owned(),
    );
    let new_graph_path = temp.path().join("new.jsonl");
    fs::write(
        &new_graph_path,
        serde_json::to_string(&new).expect("new record should serialize") + "\n",
    )
    .expect("new graph should write");

    let old_hash =
        blake3::hash(&serde_json::to_vec(&vec![old.clone()]).expect("old record should serialize"))
            .to_hex()
            .to_string();
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let stale_old_key = cli_scoped_idempotency_key("stale-old");
    fs::write(
        runtime_dir.join("idempotency.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "entries": {
                stale_old_key: {
                    "state": "pending",
                    "payload_hash": old_hash,
                    "record_ids": ["codegraph:v1:stale-pending-node"],
                    "records": [old],
                }
            }
        }))
        .expect("idempotency JSON should serialize"),
    )
    .expect("stale pending idempotency file should write");

    let mut daemon = start_daemon(&data_dir);
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&new_graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("new-write")
        .assert()
        .success();
    daemon.stop();

    let old_graph_path = temp.path().join("old.jsonl");
    fs::write(
        &old_graph_path,
        serde_json::to_string(&old).expect("old record should serialize") + "\n",
    )
    .expect("old graph should write");
    let mut restarted = start_daemon(&data_dir);
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&old_graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("stale-old")
        .assert()
        .failure()
        .stderr(predicate::str::contains("conflicting committed records"));

    let metadata = read_metadata(&data_dir);
    let read_response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/codegraph:v1:stale-pending-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        read_response.contains("\"summary\":\"new\""),
        "newer same-id write should remain current, got {read_response}"
    );

    restarted.stop();
}

#[test]
fn daemon_pending_recovery_rejects_duplicate_id_batches() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("duplicate-id.jsonl");
    let first = GraphRecord::node(
        "codegraph:v1:duplicate-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "first".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v1:duplicate-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "second".to_owned(),
    );
    fs::write(
        &graph_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first).expect("first record should serialize"),
            serde_json::to_string(&second).expect("second record should serialize")
        ),
    )
    .expect("duplicate-id graph should write");

    let records = vec![first, second];
    let payload_hash =
        blake3::hash(&serde_json::to_vec(&records).expect("duplicate records should serialize"))
            .to_hex()
            .to_string();
    let runtime_dir = runtime_dir(&data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let duplicate_pending_key = cli_scoped_idempotency_key("duplicate-pending");
    fs::write(
        runtime_dir.join("idempotency.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "entries": {
                duplicate_pending_key: {
                    "state": "pending",
                    "payload_hash": payload_hash,
                    "record_ids": [
                        "codegraph:v1:duplicate-pending-node",
                        "codegraph:v1:duplicate-pending-node"
                    ],
                    "records": records,
                }
            }
        }))
        .expect("idempotency JSON should serialize"),
    )
    .expect("pending idempotency file should write");

    let mut daemon = start_daemon(&data_dir);
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("duplicate-pending")
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate record IDs"));

    daemon.stop();
}

#[test]
fn daemon_rejects_fresh_duplicate_current_record_ids_before_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("duplicate-fresh.jsonl");
    let first = GraphRecord::node(
        "codegraph:v1:fresh-duplicate-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "first".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v1:fresh-duplicate-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "second".to_owned(),
    );
    write_graph(&graph_path, &[first, second]);
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("duplicate-fresh")
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate record IDs"));

    let metadata = read_metadata(&data_dir);
    let read_response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/codegraph:v1:fresh-duplicate-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        read_response.contains("\"record\":null"),
        "fresh duplicate rejection should happen before committing either record"
    );

    daemon.stop();
}

#[test]
fn daemon_rejects_identical_fresh_duplicate_current_record_ids_before_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("identical-duplicate-fresh.jsonl");
    let record = GraphRecord::node(
        "codegraph:v1:identical-fresh-duplicate-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "same".to_owned(),
    );
    write_graph(&graph_path, &[record.clone(), record]);
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("identical-duplicate-fresh")
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate record IDs"));

    let metadata = read_metadata(&data_dir);
    let read_response = http_request(
        &metadata.address,
        &format!(
            "GET /v1/records/codegraph:v1:identical-fresh-duplicate-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        read_response.contains("\"record\":null"),
        "identical duplicate rejection should happen before committing either record"
    );

    daemon.stop();
}

#[test]
fn daemon_rejects_identical_duplicate_edge_observations_before_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("duplicate-edge-fresh.jsonl");
    let file_id = "codegraph:v1:duplicate-edge-file".to_owned();
    let symbol_id = "codegraph:v1:duplicate-edge-symbol".to_owned();
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "file".to_owned(),
    );
    let symbol = GraphRecord::node(
        symbol_id.clone(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("thing".to_owned()),
        "symbol".to_owned(),
    );
    let edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        symbol_id,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );
    write_graph(&graph_path, &[file, symbol, edge.clone(), edge]);
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("duplicate-edge-fresh")
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate record IDs"));

    daemon.stop();
}

#[test]
fn daemon_pending_recovery_accepts_temporal_duplicate_ids() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("temporal-duplicates.jsonl");
    let first = temporal_node(
        "codegraph:v1:temporal-file",
        "1111111111111111111111111111111111111111",
        "2026-05-17T00:00:00Z",
        "first temporal observation",
    );
    let second = temporal_node(
        "codegraph:v1:temporal-file",
        "2222222222222222222222222222222222222222",
        "2026-05-18T00:00:00Z",
        "second temporal observation",
    );
    let records = vec![first, second];
    write_graph(&graph_path, &records);

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
    write_pending_idempotency(&data_dir, "temporal-pending", &records);

    let mut daemon = start_daemon(&data_dir);
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("temporal-pending")
        .assert()
        .success()
        .stdout(predicate::str::contains("idempotent: true"));

    daemon.stop();
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

#[test]
fn daemon_reports_error_when_committed_receipt_cannot_be_persisted() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let record = GraphRecord::node(
        "codegraph:v1:blocked-commit-receipt-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "blocked committed receipt".to_owned(),
    );
    let records = vec![record];
    write_graph(&graph_path, &records);
    write_pending_idempotency(&data_dir, "blocked-commit-receipt", &records);
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let idempotency_path = runtime_dir(&data_dir).join("idempotency.json");
    let blocked_tmp_path = idempotency_path.with_extension(format!("tmp.{}", metadata.pid));
    fs::create_dir(&blocked_tmp_path).expect("idempotency temp path should be blocked");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("blocked-commit-receipt")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "daemon ingest failed with HTTP 500",
        ));

    fs::remove_dir(&blocked_tmp_path).expect("blocked temp path should be removable");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--idempotency-key")
        .arg("blocked-commit-receipt")
        .assert()
        .success()
        .stdout(predicate::str::contains("idempotent: true"));

    daemon.stop();
}

#[test]
fn daemon_rejects_oversized_unauthorized_body_before_reading_it() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let response = http_request(
        &metadata.address,
        "POST /v1/status HTTP/1.1\r\nHost: egregore\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "oversized unauthorized body should be rejected with 401 (auth precedes size check), got {response}"
    );

    daemon.stop();
}

#[test]
fn daemon_rejects_unauthorized_body_under_limit_before_reading_it() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let mut stream =
        TcpStream::connect(&metadata.address).expect("daemon should accept connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should configure");
    stream
        .write_all(
            b"POST /v1/status HTTP/1.1\r\nHost: egregore\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
        )
        .expect("headers should write");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("unauthorized response should arrive before body is sent");
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "under-limit unauthorized body should be rejected before body read, got {response}"
    );

    daemon.stop();
}

#[test]
fn daemon_query_honors_positive_timeout_budget() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let record_ids = (0..1000)
        .map(|index| format!("codegraph:v1:missing-query-record-{index}"))
        .collect::<Vec<_>>();
    let response = http_json(
        &metadata,
        "POST",
        "/v1/query",
        &serde_json::json!({
            "request_id": "tiny-budget-query",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "payload": {
                "budget": { "max_results": 1000, "timeout_ms": 1 },
                "record_ids": record_ids
            }
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 408"),
        "positive timeout budget should be enforced, got {response}"
    );

    daemon.stop();
}

#[test]
fn daemon_agent_registration_distinguishes_colon_bearing_ids() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    for (request_id, agent_id, session_id) in [
        ("register-colon-1", "codex:alpha", "session"),
        ("register-colon-2", "codex", "alpha:session"),
    ] {
        let register_response = http_json(
            &metadata,
            "POST",
            "/v1/agents/register",
            &serde_json::json!({
                "request_id": request_id,
                "agent_id": agent_id,
                "session_id": session_id,
                "agent_kind": "codex",
                "project_scope": "egregore",
                "created_at": "2026-05-18T00:00:00Z"
            }),
        );
        assert!(
            register_response.starts_with("HTTP/1.1 200"),
            "colon-bearing agent/session identity should register distinctly, got {register_response}"
        );
    }

    let status = http_request(
        &metadata.address,
        &format!(
            "GET /v1/status HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    );
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "status should succeed, got {status}"
    );
    assert_eq!(response_json(&status)["agents"], 2);

    daemon.stop();
}

#[test]
fn daemon_job_ingest_retry_returns_original_job_handle() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let record = GraphRecord::node(
        "codegraph:v1:job-retry-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "job retry".to_owned(),
    );
    let request_body = serde_json::json!({
        "request_id": "retryable-job",
        "agent_id": "test-agent",
        "session_id": "test-session",
        "idempotency_key": "retryable-job-ingest",
        "domain": "codegraph",
        "created_at": "2026-05-17T00:00:00Z",
        "payload": { "records": [record] }
    });

    let first_response = http_json(&metadata, "POST", "/v1/jobs/ingest", &request_body);
    assert!(
        first_response.starts_with("HTTP/1.1 202"),
        "first job ingest should be accepted, got {first_response}"
    );
    let first_job_id = response_json(&first_response)["result"]["job_id"]
        .as_str()
        .expect("first job response should include id")
        .to_owned();
    thread::sleep(Duration::from_millis(5));
    let mut retry_body_value = request_body;
    retry_body_value["request_id"] = serde_json::json!("retryable-job-fresh-request");
    let retry_response = http_json(&metadata, "POST", "/v1/jobs/ingest", &retry_body_value);
    assert!(
        retry_response.starts_with("HTTP/1.1 200"),
        "job ingest idempotent replay should return 200, got {retry_response}"
    );
    let retry_body = response_json(&retry_response);
    let retry_job_id = retry_body["result"]["job_id"]
        .as_str()
        .expect("retry job response should include id");

    assert_eq!(retry_job_id, first_job_id);
    let job_status = wait_for_job(&metadata, &first_job_id);
    assert_eq!(job_status["status"], "completed");
    assert_eq!(job_status["report"]["failed"], 0);

    daemon.stop();
}

#[test]
fn daemon_job_ingest_rejects_same_key_with_different_payload() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let first_record = GraphRecord::node(
        "codegraph:v1:job-conflict-first".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "job conflict first".to_owned(),
    );
    let second_record = GraphRecord::node(
        "codegraph:v1:job-conflict-second".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "job conflict second".to_owned(),
    );
    let first_body = serde_json::json!({
        "request_id": "conflicting-job",
        "agent_id": "test-agent",
        "session_id": "test-session",
        "idempotency_key": "conflicting-job-ingest",
        "domain": "codegraph",
        "created_at": "2026-05-17T00:00:00Z",
        "payload": { "records": [first_record] }
    });
    let second_body = serde_json::json!({
        "request_id": "conflicting-job",
        "agent_id": "test-agent",
        "session_id": "test-session",
        "idempotency_key": "conflicting-job-ingest",
        "domain": "codegraph",
        "created_at": "2026-05-17T00:00:00Z",
        "payload": { "records": [second_record] }
    });

    let first_response = http_json(&metadata, "POST", "/v1/jobs/ingest", &first_body);
    assert!(
        first_response.starts_with("HTTP/1.1 202"),
        "first job ingest should be accepted, got {first_response}"
    );
    let second_response = http_json(&metadata, "POST", "/v1/jobs/ingest", &second_body);
    assert!(
        second_response.starts_with("HTTP/1.1 409"),
        "same job idempotency key with different payload should conflict, got {second_response}"
    );

    daemon.stop();
}

#[test]
fn daemon_ingest_idempotency_keys_are_scoped_by_agent_session() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let shared_key = "shared-scan-key";
    let first_record = GraphRecord::node(
        "codegraph:v1:agent-scope-first".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "agent scoped first".to_owned(),
    );
    let second_record = GraphRecord::node(
        "codegraph:v1:agent-scope-second".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "agent scoped second".to_owned(),
    );
    for (request_id, agent_id, session_id, record) in [
        ("agent-scope-1", "agent-a", "session", first_record),
        ("agent-scope-2", "agent", "a:session", second_record),
    ] {
        let response = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": request_id,
                "agent_id": agent_id,
                "session_id": session_id,
                "idempotency_key": shared_key,
                "domain": "codegraph",
                "created_at": "2026-05-17T00:00:00Z",
                "payload": { "records": [record] }
            }),
        );
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "same local idempotency key should be independent per agent session, got {response}"
        );
        assert_eq!(response_json(&response)["result"]["failed"], 0);
    }

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
            "project_scope": "egregore",
            "created_at": "2026-05-17T00:00:00Z"
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
    let job_id = response_json(&job_response)["result"]["job_id"]
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
            "payload": {
                "budget": { "max_results": 1 },
                "record_ids": [first_record_id]
            }
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

fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).or_else(|symlink_error| {
            let status = ProcessCommand::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(symlink_error)
            }
        })
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
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

fn write_graph(graph_path: &Path, records: &[GraphRecord]) {
    let jsonl = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("record should serialize"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(graph_path, jsonl).expect("graph should write");
}

fn write_pending_idempotency(data_dir: &Path, idempotency_key: &str, records: &[GraphRecord]) {
    let runtime_dir = runtime_dir(data_dir);
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    let payload_hash =
        blake3::hash(&serde_json::to_vec(records).expect("pending records should serialize"))
            .to_hex()
            .to_string();
    let record_ids = records.iter().map(GraphRecord::id).collect::<Vec<_>>();
    let idempotency_key = cli_scoped_idempotency_key(idempotency_key);
    fs::write(
        runtime_dir.join("idempotency.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "entries": {
                idempotency_key: {
                    "state": "pending",
                    "payload_hash": payload_hash,
                    "record_ids": record_ids,
                    "records": records,
                }
            }
        }))
        .expect("pending idempotency JSON should serialize"),
    )
    .expect("pending idempotency file should write");
}

fn cli_scoped_idempotency_key(idempotency_key: &str) -> String {
    scoped_test_idempotency_key("egregore-cli", "records/ingest", idempotency_key)
}

fn scoped_test_idempotency_key(agent_id: &str, route: &str, idempotency_key: &str) -> String {
    let mut key = String::new();
    key.push_str("idempotency:");
    key.push_str(&agent_id.len().to_string());
    key.push(':');
    key.push_str(&route.len().to_string());
    key.push(':');
    key.push_str(&idempotency_key.len().to_string());
    key.push(':');
    key.push_str(agent_id);
    key.push_str(route);
    key.push_str(idempotency_key);
    key
}

fn temporal_node(id: &str, git_commit: &str, valid_time: &str, summary: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        summary.to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: git_commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
    })
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
            let result = &body["result"];
            if result["status"] == "completed" || result["status"] == "failed" {
                return result.clone();
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

fn http_get_authed(metadata: &DaemonMetadata, path: &str) -> String {
    http_request(
        &metadata.address,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    )
}

fn http_post_empty(metadata: &DaemonMetadata, path: &str) -> String {
    http_request(
        &metadata.address,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            metadata.token
        ),
    )
}

/// Contract conformance: table-driven assertions over every daemon route.
///
/// Each assertion group maps to one of the five checks in issue #5 ACC item 8:
///   (a) missing required envelope field → `missing_field` + field path
///   (b) unknown domain → `invalid_domain`
///   (c) idempotency replay same payload → HTTP 200 + original result
///   (d) idempotency conflict different payload → `idempotency_conflict` + HTTP 409
///   (e) success envelope is `{ ok: true, request_id, result }` with no error key
///
/// Adding a new route to the daemon requires a new block here.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
#[allow(clippy::too_many_lines)]
fn contract_conformance_all_routes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // ── GET /v1/health ──────────────────────────────────────────────────────────
    // (e) success + api_version surfaced
    {
        let res = http_request(
            &metadata.address,
            "GET /v1/health HTTP/1.1\r\nHost: egregore\r\nConnection: close\r\n\r\n",
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "GET /v1/health should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["api_version"], "v1",
            "GET /v1/health must include api_version: \"v1\", got {body}"
        );
    }

    // ── GET /v1/status ──────────────────────────────────────────────────────────
    // (e) success + api_version surfaced
    {
        let res = http_get_authed(&metadata, "/v1/status");
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "GET /v1/status should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["api_version"], "v1",
            "GET /v1/status must include api_version: \"v1\", got {body}"
        );
        assert!(
            body.get("idempotency_store_size").is_some(),
            "GET /v1/status must include idempotency_store_size, got {body}"
        );
    }

    // ── POST /v1/records/ingest ─────────────────────────────────────────────────
    let ingest_record = serde_json::json!({
        "record_type": "node",
        "id": "codegraph:v1:conformance-ingest-node",
        "schema_version": 1,
        "kind": "Repository",
        "name": "conformance",
        "summary": "conformance test node"
    });

    // (a) missing request_id
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-ingest-key",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [ingest_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/records/ingest missing request_id should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], false,
            "error envelope must have ok:false, got {body}"
        );
        assert_eq!(
            body["error"]["code"], "missing_field",
            "missing request_id should return code missing_field, got {body}"
        );
        assert_eq!(
            body["error"]["field"], "request_id",
            "missing_field error must name the field, got {body}"
        );
        assert!(
            body.get("result").is_none(),
            "error envelope must not have a result key, got {body}"
        );
    }

    // (a) missing idempotency_key
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": "conf-ingest-missing-ikey",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [ingest_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/records/ingest missing idempotency_key should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], false,
            "error envelope must have ok:false, got {body}"
        );
        assert_eq!(
            body["error"]["code"], "missing_field",
            "missing idempotency_key should return code missing_field, got {body}"
        );
        assert_eq!(
            body["error"]["field"], "idempotency_key",
            "missing_field error must name the field, got {body}"
        );
    }

    // (b) unknown domain
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": "conf-ingest-baddomain",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-ingest-baddomain-key",
                "domain": "unknown_domain_xyz",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [ingest_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/records/ingest unknown domain should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], false,
            "error envelope must have ok:false, got {body}"
        );
        assert_eq!(
            body["error"]["code"], "invalid_domain",
            "unknown domain should return code invalid_domain, got {body}"
        );
    }

    // (e) success envelope shape
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": "conf-ingest-success",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-ingest-success-key",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [ingest_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/records/ingest valid request should be 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "success envelope must have ok:true, got {body}"
        );
        assert_eq!(
            body["request_id"], "conf-ingest-success",
            "success envelope must echo request_id, got {body}"
        );
        assert!(
            body.get("result").is_some(),
            "success envelope must have result key, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "success envelope must not have error key, got {body}"
        );
    }

    // (c) idempotency replay same payload → HTTP 200 + result.idempotent=true
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": "conf-ingest-replay-2",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-ingest-success-key",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [ingest_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/records/ingest idempotent replay should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "idempotent replay envelope must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["idempotent"], true,
            "idempotent replay result must have idempotent:true, got {body}"
        );
    }

    // (d) idempotency conflict different payload → HTTP 409 + idempotency_conflict
    {
        let other_record = serde_json::json!({
            "record_type": "node",
            "id": "codegraph:v1:conformance-conflict-node",
            "schema_version": 1,
            "kind": "Repository",
            "name": "conflict",
            "summary": "a different node"
        });
        let res = http_json(
            &metadata,
            "POST",
            "/v1/records/ingest",
            &serde_json::json!({
                "request_id": "conf-ingest-conflict",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-ingest-success-key",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": [other_record]}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 409"),
            "POST /v1/records/ingest conflict should return 409, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], false,
            "conflict envelope must have ok:false, got {body}"
        );
        assert_eq!(
            body["error"]["code"], "idempotency_conflict",
            "conflict should return code idempotency_conflict, got {body}"
        );
    }

    // ── POST /v1/query ──────────────────────────────────────────────────────────
    // (a) missing request_id
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "agent_id": "test-agent",
                "session_id": "test-session",
                "payload": { "record_ids": [] }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/query missing request_id should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], false,
            "error envelope must have ok:false, got {body}"
        );
        assert_eq!(
            body["error"]["code"], "missing_field",
            "missing request_id on query should return missing_field, got {body}"
        );
        assert_eq!(body["error"]["field"], "request_id");
    }

    // (e) success envelope
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "conf-query-success",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "payload": { "record_ids": [] }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/query valid request should be 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "query success envelope must have ok:true, got {body}"
        );
        assert_eq!(body["request_id"], "conf-query-success");
        assert!(
            body.get("result").is_some(),
            "query success must have result, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "query success must not have error, got {body}"
        );
    }

    // ── POST /v1/agents/register ────────────────────────────────────────────────
    // (a) missing request_id
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/agents/register",
            &serde_json::json!({
                "agent_id": "conf-agent",
                "session_id": "conf-session",
                "agent_kind": "test",
                "project_scope": "egregore"
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/agents/register missing request_id should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "missing_field");
        assert_eq!(body["error"]["field"], "request_id");
    }

    // (e) success envelope
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/agents/register",
            &serde_json::json!({
                "request_id": "conf-register-success",
                "agent_id": "conf-agent",
                "session_id": "conf-session",
                "agent_kind": "test",
                "project_scope": "egregore",
                "created_at": "2026-05-18T00:00:00Z"
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/agents/register valid request should be 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "register success envelope must have ok:true, got {body}"
        );
        assert_eq!(body["request_id"], "conf-register-success");
        assert!(
            body.get("result").is_some(),
            "register success must have result, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "register success must not have error, got {body}"
        );
    }

    // ── POST /v1/agents/heartbeat ───────────────────────────────────────────────
    // (a) missing request_id
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/agents/heartbeat",
            &serde_json::json!({
                "agent_id": "conf-agent",
                "session_id": "conf-session"
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/agents/heartbeat missing request_id should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "missing_field");
        assert_eq!(body["error"]["field"], "request_id");
    }

    // (e) success envelope
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/agents/heartbeat",
            &serde_json::json!({
                "request_id": "conf-heartbeat-success",
                "agent_id": "conf-agent",
                "session_id": "conf-session"
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/agents/heartbeat valid request should be 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "heartbeat success envelope must have ok:true, got {body}"
        );
        assert_eq!(body["request_id"], "conf-heartbeat-success");
        assert!(body.get("result").is_some());
        assert!(body.get("error").is_none());
    }

    // ── POST /v1/jobs/ingest ────────────────────────────────────────────────────
    // (a) missing idempotency_key
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/jobs/ingest",
            &serde_json::json!({
                "request_id": "conf-job-missing-ikey",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": []}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/jobs/ingest missing idempotency_key should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "missing_field");
        assert_eq!(body["error"]["field"], "idempotency_key");
    }

    // (b) unknown domain
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/jobs/ingest",
            &serde_json::json!({
                "request_id": "conf-job-baddomain",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-job-baddomain-key",
                "domain": "unknown_domain",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": []}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "POST /v1/jobs/ingest unknown domain should be 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "invalid_domain");
    }

    // (e) success envelope (202 Accepted)
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/jobs/ingest",
            &serde_json::json!({
                "request_id": "conf-job-success",
                "agent_id": "test-agent",
                "session_id": "test-session",
                "idempotency_key": "conf-job-success-key",
                "domain": "codegraph",
                "created_at": "2026-05-18T00:00:00Z",
                "payload": {"records": []}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 202"),
            "POST /v1/jobs/ingest valid request should be 202, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "job ingest success envelope must have ok:true, got {body}"
        );
        assert_eq!(body["request_id"], "conf-job-success");
        assert!(
            body.get("result").is_some(),
            "job ingest must have result, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "job ingest must not have error, got {body}"
        );
    }

    // ── POST /v1/admin/checkpoint ───────────────────────────────────────────────
    // (e) success envelope (no request body, request_id is null)
    {
        let res = http_post_empty(&metadata, "/v1/admin/checkpoint");
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "POST /v1/admin/checkpoint should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "checkpoint success envelope must have ok:true, got {body}"
        );
        assert!(
            body.get("result").is_some(),
            "checkpoint must have result, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "checkpoint must not have error, got {body}"
        );
    }

    // ── GET /v1/records/{id} ────────────────────────────────────────────────────
    // (e) success envelope (record may be null if not present)
    {
        let res = http_get_authed(
            &metadata,
            "/v1/records/codegraph:v1:conformance-ingest-node",
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "GET /v1/records/{{id}} should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "record read success envelope must have ok:true, got {body}"
        );
        assert!(
            body.get("result").is_some(),
            "record read must have result, got {body}"
        );
        assert!(
            body.get("error").is_none(),
            "record read must not have error, got {body}"
        );
    }

    daemon.stop();
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

// ── Schema conformance: issue #6 ─────────────────────────────────────────────

// (a) Every NodeKind variant is either code-graph-documented,
// agent-memory-documented, or agent-memory-reserved.
// The exhaustive match enforces this at compile time: adding a new
// NodeKind variant without updating this list is a compile error.
#[test]
fn all_node_kinds_have_documented_schema() {
    let _ = |k: NodeKind| match k {
        // Documented in docs/prd/0001-codebase-knowledge-graph.md
        NodeKind::Repository
        | NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::Commit
        | NodeKind::Change
        | NodeKind::SemanticDrift => "code-graph-documented",
        // Documented in docs/schema/agent-memory.md (full schema)
        NodeKind::Agent | NodeKind::AgentSession | NodeKind::Observation => {
            "agent-memory-documented"
        }
        // Reserved with one-line definitions in docs/schema/agent-memory.md
        NodeKind::Task
        | NodeKind::Artifact
        | NodeKind::Verification
        | NodeKind::CommandEvidence => "agent-memory-reserved",
    };
}

// (b) Every EdgeLabel variant is either code-graph-internal or has a row in
// the cross-domain edge registry in docs/schema/agent-memory.md.
// The exhaustive match enforces this at compile time.
#[test]
fn all_edge_labels_have_documented_schema() {
    let _ = |l: EdgeLabel| match l {
        // Code-graph-internal: documented in docs/prd/0001-codebase-knowledge-graph.md
        EdgeLabel::Contains
        | EdgeLabel::Defines
        | EdgeLabel::Imports
        | EdgeLabel::References
        | EdgeLabel::Calls
        | EdgeLabel::Implements
        | EdgeLabel::Mentions
        | EdgeLabel::ChangedIn
        | EdgeLabel::ParentOf
        | EdgeLabel::DriftsFrom => "code-graph-internal",
        // Cross-domain registry: documented in docs/schema/agent-memory.md
        EdgeLabel::SessionOf
        | EdgeLabel::AuthoredBy
        | EdgeLabel::HasEvidence
        | EdgeLabel::Observes
        | EdgeLabel::MentionsSymbol
        | EdgeLabel::TouchedFile
        | EdgeLabel::ProducedPatch
        | EdgeLabel::ValidatedBy
        | EdgeLabel::FailedOn
        | EdgeLabel::ExplainsChange
        | EdgeLabel::ReferencesTask
        | EdgeLabel::Contradicts
        | EdgeLabel::Supersedes
        | EdgeLabel::RelatesTo => "cross-domain-registry",
    };
}

// (c) Agent and AgentSession records produced by POST /v1/agents/register
// use the documented agent_memory:v1: ID prefix and return the documented
// field set.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn agent_registration_produces_agent_memory_ids() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/agents/register",
        &serde_json::json!({
            "request_id": "schema-shape-test",
            "agent_id": "test-agent-schema",
            "session_id": "test-session-schema",
            "agent_kind": "claude-code",
            "project_scope": "egregore",
            "created_at": "2026-05-18T00:00:00Z"
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "agent registration should succeed, got {response}"
    );

    let body = response_json(&response);
    let result = &body["result"];

    assert_eq!(
        result["status"], "registered",
        "agent registration must return status=registered per schema doc"
    );

    let record_ids = result["record_ids"]
        .as_array()
        .expect("record_ids must be an array per schema doc");
    assert!(
        record_ids.len() >= 2,
        "agent registration must produce at least Agent and AgentSession records"
    );

    let all_agent_memory = record_ids.iter().all(|id| {
        id.as_str()
            .is_some_and(|s| s.starts_with("agent_memory:v1:"))
    });
    assert!(
        all_agent_memory,
        "all agent-registration record IDs must use agent_memory:v1: prefix per schema doc, got {record_ids:?}"
    );

    let node_kinds = result["node_kinds"]
        .as_array()
        .expect("node_kinds must be an array per schema doc");
    assert!(
        node_kinds.contains(&serde_json::Value::String("Agent".to_owned())),
        "node_kinds must include Agent, got {node_kinds:?}"
    );
    assert!(
        node_kinds.contains(&serde_json::Value::String("AgentSession".to_owned())),
        "node_kinds must include AgentSession, got {node_kinds:?}"
    );

    daemon.stop();
}

// (d) An evidence link whose target_record_id does not exist in the store
// is rejected with the documented unresolved_evidence_target error code.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn evidence_link_with_missing_target_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "evidence-link-missing-target",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "evidence-link-missing-target-key",
            "domain": "agent_memory",
            "created_at": "2026-05-18T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:evidence-link-test-obs",
                    "kind": "Observation",
                    "schema_version": 1,
                    "text": "test observation",
                    "agent_id": "test-agent",
                    "agent_kind": "coding",
                    "session_id": "test-session",
                    "observed_at": "2026-05-18T00:00:00Z",
                    "ingested_at": "2026-05-18T00:00:00Z",
                    "confidence": "0.9",
                    "summary": "test observation with unresolved evidence link",
                    "evidence_links": [{
                        "target_record_id": "codegraph:v1:nonexistent-symbol-xyzzy",
                        "target_domain": "codegraph",
                        "relation": "OBSERVES",
                        "confidence": "0.9"
                    }]
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ingest with unresolved evidence link target should be rejected, got {response}"
    );

    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "unresolved_evidence_target",
        "rejection must carry unresolved_evidence_target code per schema doc, got {body}"
    );

    daemon.stop();
}
