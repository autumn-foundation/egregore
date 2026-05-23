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
    adapters::{EmbeddedAletheiaSink, GraphSink},
    daemon::{DaemonClient, DaemonMetadata as ClientDaemonMetadata, StoreLease},
    import_traj,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, IdentitySource, NodeKind,
        PROJECT_SCHEMA_VERSION, RepositoryIdentityPayload, SCHEMA_VERSION, SEMANTIC_SCHEMA_VERSION,
        TemporalMetadata,
    },
    traj::ImportOptions,
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
            "codegraph:v3:wrong-service-ingest-node".to_owned(),
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
        "codegraph:v3:restart-recovery-node".to_owned(),
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
        "codegraph:v3:restart-recovery-node".to_owned(),
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
        "codegraph:v3:restart-recovery-node".to_owned(),
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
        "codegraph:v3:same-id-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "old".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v3:same-id-node".to_owned(),
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
        "record_ids": ["codegraph:v3:same-id-node"],
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
            "GET /v1/records/codegraph:v3:same-id-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
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
        "codegraph:v3:stale-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "old".to_owned(),
    );
    let new = GraphRecord::node(
        "codegraph:v3:stale-pending-node".to_owned(),
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
                    "record_ids": ["codegraph:v3:stale-pending-node"],
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
            "GET /v1/records/codegraph:v3:stale-pending-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
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
        "codegraph:v3:duplicate-pending-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "first".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v3:duplicate-pending-node".to_owned(),
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
                        "codegraph:v3:duplicate-pending-node",
                        "codegraph:v3:duplicate-pending-node"
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
        "codegraph:v3:fresh-duplicate-node".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "first".to_owned(),
    );
    let second = GraphRecord::node(
        "codegraph:v3:fresh-duplicate-node".to_owned(),
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
            "GET /v1/records/codegraph:v3:fresh-duplicate-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
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
        "codegraph:v3:identical-fresh-duplicate-node".to_owned(),
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
            "GET /v1/records/codegraph:v3:identical-fresh-duplicate-node HTTP/1.1\r\nHost: egregore\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
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
    let file_id = "codegraph:v3:duplicate-edge-file".to_owned();
    let symbol_id = "codegraph:v3:duplicate-edge-symbol".to_owned();
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
        "codegraph:v3:temporal-file",
        "1111111111111111111111111111111111111111",
        "2026-05-17T00:00:00Z",
        "first temporal observation",
    );
    let second = temporal_node(
        "codegraph:v3:temporal-file",
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
        "codegraph:v3:blocked-commit-receipt-node".to_owned(),
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
        .map(|index| format!("codegraph:v3:missing-query-record-{index}"))
        .collect::<Vec<_>>();
    let response = http_json(
        &metadata,
        "POST",
        "/v1/query",
        &serde_json::json!({
            "request_id": "tiny-budget-query",
            "agent_id": "test-agent",
            "verb": "get_records",
            "params": { "record_ids": record_ids },
            "budget": { "max_results": 1000, "timeout_ms": 1 }
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
        "codegraph:v3:job-retry-node".to_owned(),
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
        "codegraph:v3:job-conflict-first".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "job conflict first".to_owned(),
    );
    let second_record = GraphRecord::node(
        "codegraph:v3:job-conflict-second".to_owned(),
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
        "codegraph:v3:agent-scope-first".to_owned(),
        NodeKind::Module,
        None,
        None,
        Some("repo".to_owned()),
        "agent scoped first".to_owned(),
    );
    let second_record = GraphRecord::node(
        "codegraph:v3:agent-scope-second".to_owned(),
        NodeKind::Module,
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
        .arg("--repo-id-override")
        .arg("fixture-rust-basic-stable")
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
            "verb": "get_records",
            "params": { "record_ids": [&first_record_id] },
            "budget": { "max_results": 1 }
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
        valid_time_source: None,
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

fn read_repo_text(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("{path} should be readable: {error}"))
}

const PROJECT_EXTERNAL_LINK_ID: &str = "project:v1:test-external-link";
const PROJECT_TASK_ID: &str = "project:v1:test-task";

fn project_external_link_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "ExternalLink",
        "schema_version": PROJECT_SCHEMA_VERSION,
        "domain": "project",
        "entity_id": id,
        "system": "github",
        "url": "https://github.com/madmax983/egregore/issues/14",
        "system_native_id": "14",
        "repository_remote": "https://github.com/madmax983/egregore",
        "discovered_at": "2026-05-18T05:16:46Z",
        "valid_time": "2026-05-18T05:49:32Z",
        "valid_time_source": "github_updated_at",
        "transaction_time": "2026-05-22T00:00:00Z",
        "summary": "External GitHub handle for issue 14"
    })
}

fn project_task_json(id: &str, status: &str, transaction_time: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "Task",
        "schema_version": PROJECT_SCHEMA_VERSION,
        "domain": "project",
        "entity_id": id,
        "title": "Spec Task/AcceptanceCriterion shapes before any project-graph writer",
        "body_handle": {
            "inline": "Issue body truncated in fixture",
            "hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bytes": 31
        },
        "status": status,
        "source_kind": "github_issue",
        "source_external_link_id": PROJECT_EXTERNAL_LINK_ID,
        "assignees": ["markm"],
        "labels": ["spec", "pm"],
        "priority": "normal",
        "confidence": "1.0",
        "valid_time": "2026-05-18T05:49:32Z",
        "valid_time_source": "github_updated_at",
        "transaction_time": transaction_time,
        "summary": format!("Project task issue 14 status {status}")
    })
}

fn project_acceptance_criterion_json(
    id: &str,
    parent_task_id: &str,
    status: &str,
    verification_link_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "AcceptanceCriterion",
        "schema_version": PROJECT_SCHEMA_VERSION,
        "domain": "project",
        "entity_id": id,
        "parent_task_id": parent_task_id,
        "ordinal": 1,
        "text": "A new doc docs/schema/project-graph.md exists.",
        "status": status,
        "verification_link_id": verification_link_id,
        "confidence": "1.0",
        "valid_time": "2026-05-18T05:49:32Z",
        "valid_time_source": "github_updated_at",
        "transaction_time": "2026-05-22T00:00:02Z",
        "summary": format!("Acceptance criterion for {parent_task_id}")
    })
}

fn verification_record_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "Verification",
        "schema_version": 1,
        "domain": "verification",
        "source_artifact_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "source_artifact_path": "tests/daemon.rs",
        "executed_at": "2026-05-22T00:00:00Z",
        "verification_kind": "manual",
        "status": "passed",
        "summary": "Manual verification fixture"
    })
}

fn codegraph_file_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "File",
        "schema_version": SCHEMA_VERSION,
        "repo_relative_path": "src/lib.rs",
        "name": "src/lib.rs",
        "summary": "Fixture codegraph file"
    })
}

const PATCH_PRODUCER_SESSION_ID: &str = "agent_memory:v1:producer-session";

fn agent_session_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "AgentSession",
        "schema_version": 1,
        "agent_id": "test-agent",
        "agent_kind": "codex",
        "session_id": "test-session",
        "observed_at": "2026-05-22T00:00:00Z",
        "ingested_at": "2026-05-22T00:00:00Z",
        "name": "Test session",
        "summary": "Test AgentSession"
    })
}

fn agent_turn_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "AgentTurn",
        "schema_version": 1,
        "agent_id": "test-agent",
        "agent_kind": "codex",
        "session_id": "test-session",
        "observed_at": "2026-05-22T00:00:00Z",
        "ingested_at": "2026-05-22T00:00:00Z",
        "summary": "Test AgentTurn"
    })
}

fn valid_tool_call_json(id: &str, linked_turn_id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "ToolCall",
        "schema_version": 1,
        "domain": "agent_memory",
        "agent_id": "test-agent",
        "agent_kind": "codex",
        "session_id": "test-session",
        "observed_at": "2026-05-22T00:00:00Z",
        "ingested_at": "2026-05-22T00:00:00Z",
        "summary": "ToolCall fixture",
        "source_artifact_path": "fixtures/session.traj",
        "source_artifact_hash": "1111111111111111111111111111111111111111111111111111111111111111",
        "linked_turn_id": linked_turn_id,
        "tool_name": "Bash",
        "tool_kind": "bash",
        "arguments_summary": "cargo test",
        "arguments_handle": {
            "hash": "2222222222222222222222222222222222222222222222222222222222222222",
            "bytes": 10,
            "inline": "cargo test"
        },
        "started_at": "2026-05-22T00:00:00Z",
        "finished_at": "2026-05-22T00:00:01Z",
        "status": "succeeded"
    })
}

fn valid_file_edit_json(id: &str, linked_turn_id: &str) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "FileEdit",
        "schema_version": 1,
        "domain": "agent_memory",
        "agent_id": "test-agent",
        "agent_kind": "codex",
        "session_id": "test-session",
        "observed_at": "2026-05-22T00:00:00Z",
        "ingested_at": "2026-05-22T00:00:00Z",
        "summary": "FileEdit fixture",
        "source_artifact_path": "fixtures/session.traj",
        "source_artifact_hash": "1111111111111111111111111111111111111111111111111111111111111111",
        "repo_relative_path": "src/lib.rs",
        "edit_kind": "modify",
        "before_hash": "3333333333333333333333333333333333333333333333333333333333333333",
        "after_hash": "4444444444444444444444444444444444444444444444444444444444444444",
        "hunk_count": 1,
        "linked_turn_id": linked_turn_id
    })
}

fn ingest_patch_producer_session(metadata: &DaemonMetadata, idempotency_key: &str) {
    let response = http_json(
        metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": format!("seed-{idempotency_key}"),
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": idempotency_key,
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_session_json(PATCH_PRODUCER_SESSION_ID)] }
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "producer AgentSession seed should succeed, got {response}"
    );
}

fn patch_artifact_fixture(
    id: &str,
    patch_status: &str,
    patch_handle: &serde_json::Value,
) -> serde_json::Value {
    let patch_bytes_size = patch_handle
        .get("inline")
        .and_then(serde_json::Value::as_str)
        .map_or(32_u64, |inline| inline.len() as u64);
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "PatchArtifact",
        "schema_version": 1,
        "domain": "artifact",
        "summary": format!("PatchArtifact status={patch_status}"),
        "patch_status": patch_status,
        "base_commit": null,
        "unknown_base_reason": "unknown_base",
        "target_files": [],
        "patch_bytes_hash": "0000000000000000000000000000000000000000000000000000000000000000",
        "patch_bytes_size": patch_bytes_size,
        "patch_handle": patch_handle,
        "validation_summary": "fixture patch status",
        "source_artifact_path": "fixtures/session.traj",
        "source_artifact_hash": "1111111111111111111111111111111111111111111111111111111111111111",
        "producer_session_id": PATCH_PRODUCER_SESSION_ID,
        "valid_time": "2026-05-21T00:00:00Z",
        "valid_time_source": "produced_at",
        "ingested_at": "2026-05-21T00:00:00Z"
    })
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
        "id": "codegraph:v3:conformance-ingest-node",
        "schema_version": 1,
        "kind": "Module",
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
            "id": "codegraph:v3:conformance-conflict-node",
            "schema_version": 1,
            "kind": "Module",
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
                "verb": "get_records",
                "params": { "record_ids": [] }
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

    // (e) success envelope — verb envelope format
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "conf-query-success",
                "agent_id": "test-agent",
                "verb": "get_records",
                "params": { "record_ids": [] }
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
                "agent_kind": "other",
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
            "/v1/records/codegraph:v3:conformance-ingest-node",
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
// agent-memory-documented, agent-memory-reserved, project-domain-documented,
// project-domain-reserved, or verification-domain-documented.
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
        | NodeKind::Change => "code-graph-documented",
        // Documented in docs/schema/semantic-drift.md
        NodeKind::SemanticDrift | NodeKind::EmbeddingModel | NodeKind::EmbeddingVector => {
            "semantic-domain-documented"
        }
        // Documented in docs/schema/agent-memory.md (full schema)
        NodeKind::Agent | NodeKind::AgentSession | NodeKind::Observation => {
            "agent-memory-documented"
        }
        // Project-domain day-one shapes documented in docs/schema/project-graph.md
        NodeKind::Task | NodeKind::AcceptanceCriterion | NodeKind::ExternalLink => {
            "project-domain-documented"
        }
        // Project-domain reserved shapes documented in docs/schema/project-graph.md
        NodeKind::Product
        | NodeKind::Project
        | NodeKind::Plan
        | NodeKind::GitHubIssue
        | NodeKind::PR
        | NodeKind::Review
        | NodeKind::LocalTask => "project-domain-reserved",
        // Reserved with one-line definitions in docs/schema/agent-memory.md §4b
        NodeKind::Artifact | NodeKind::CommandEvidence => "agent-memory-reserved",
        // M2 trajectory-importer node kinds (docs/schema/agent-memory.md §4b + PRD M2)
        NodeKind::AgentRun | NodeKind::AgentTurn | NodeKind::Failure | NodeKind::Decision => {
            "agent-memory-m2-traj-importer"
        }
        NodeKind::ToolCall | NodeKind::FileEdit | NodeKind::PatchArtifact => {
            "agent-actions-documented"
        }
        // Documented in docs/schema/verification.md (full schema, day-one shapes)
        NodeKind::Verification => "verification-documented",
        // Reserved in docs/schema/verification.md §5 with one-line definitions
        NodeKind::CommandRun
        | NodeKind::TestRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => "verification-domain-documented",
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
        | EdgeLabel::ParentOf => "code-graph-internal",
        // Semantic drift registry: documented in docs/schema/semantic-drift.md
        EdgeLabel::DriftsFrom | EdgeLabel::DriftsPrior | EdgeLabel::MeasuredBy => {
            "semantic-domain-registry"
        }
        // Cross-domain registry: documented in docs/schema/agent-memory.md
        EdgeLabel::SessionOf
        | EdgeLabel::AuthoredBy
        | EdgeLabel::HasEvidence
        | EdgeLabel::Observes
        | EdgeLabel::MentionsSymbol
        | EdgeLabel::TouchedFile
        | EdgeLabel::ProducedPatch
        | EdgeLabel::ProducedEvidence
        | EdgeLabel::ValidatedBy
        | EdgeLabel::ClosesAcceptanceCriterion
        | EdgeLabel::OwnedByTask
        | EdgeLabel::ExternalHandle
        | EdgeLabel::TouchesFile
        | EdgeLabel::FailedOn
        | EdgeLabel::ExplainsChange
        | EdgeLabel::ReferencesTask
        | EdgeLabel::Contradicts
        | EdgeLabel::Supersedes
        | EdgeLabel::RelatesTo => "cross-domain-registry",
    };
}

#[test]
fn project_graph_schema_doc_is_cross_linked_and_names_day_one_contract() {
    let schema = read_repo_text("docs/schema/project-graph.md");
    for needle in [
        "# Project-Graph Domain Schema - v1",
        "schema_version` = `1`",
        "intent-shaped, externally-anchored when possible",
        "Task record shape",
        "AcceptanceCriterion record shape",
        "ExternalLink record shape",
        "acceptance_criterion_missing_verification",
        "one AC per top-level checklist item under the `Acceptance Criteria` heading",
        "project:v<schema_version>:<blake3(domain || kind || source_kind || source_native_id || entity_kind_identity)>",
        "append-with-same-entity-id",
        "Product` | Long-lived product/repository initiative",
        "LocalTask` | Named in the PRD as a sibling of `GitHubIssue`",
    ] {
        assert!(
            schema.contains(needle),
            "project-graph schema must document `{needle}`"
        );
    }

    for path in [
        "README.md",
        "docs/prd/0000-egregore-vision.md",
        "docs/schema/agent-memory.md",
        "docs/schema/verification.md",
    ] {
        let text = read_repo_text(path);
        assert!(
            text.contains("docs/schema/project-graph.md") || text.contains("project-graph.md"),
            "{path} must link to docs/schema/project-graph.md"
        );
    }
}

#[test]
fn project_graph_edge_registry_rows_are_documented() {
    let registry = read_repo_text("docs/schema/agent-memory.md");
    for needle in [
        "| `REFERENCES_TASK` | `agent_memory` | `project` | `Observation`, `Decision`, `Failure`, `Lesson` | `Task` | many:many | no |",
        "| `CLOSES_ACCEPTANCE_CRITERION` | `project` | `verification` | `AcceptanceCriterion` | `Verification`, `CommandRun`, `TestRun` | many:1 | no |",
        "| `OWNED_BY_TASK` | `project` | `project` | `AcceptanceCriterion` | `Task` | many:1 | no |",
        "| `EXTERNAL_HANDLE` | `project` | `project` | `Task`, `AcceptanceCriterion` | `ExternalLink` | many:1 | no |",
        "| `TOUCHES_FILE` | `project` | `codegraph` | `Task` | `File` | many:many | no |",
        "| `MENTIONS_SYMBOL` | `project` | `codegraph` | `Task` | `Symbol` | many:many | yes |",
    ] {
        assert!(
            registry.contains(needle),
            "agent-memory edge registry must contain exact project row: {needle}"
        );
    }
}

#[test]
fn project_acceptance_criterion_verified_requires_verification_link() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-ac-missing-verification",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-ac-missing-verification-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_external_link_json(PROJECT_EXTERNAL_LINK_ID),
                    project_task_json(PROJECT_TASK_ID, "open", "2026-05-22T00:00:01Z"),
                    project_acceptance_criterion_json(
                        "project:v1:test-ac-missing-verification",
                        PROJECT_TASK_ID,
                        "verified",
                        None
                    )
                ]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 422"),
        "verified AC missing verification should be rejected with 422, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "acceptance_criterion_missing_verification",
        "verified AC missing verification must use documented error code, got {body}"
    );

    daemon.stop();
}

#[test]
fn project_acceptance_criterion_parent_must_exist() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-ac-missing-parent",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-ac-missing-parent-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_acceptance_criterion_json(
                        "project:v1:test-ac-missing-parent",
                        "project:v1:missing-task",
                        "unverified",
                        None
                    )
                ]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 422"),
        "AC with missing parent task should be rejected with 422, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "unresolved_evidence_target",
        "missing parent task must use unresolved_evidence_target, got {body}"
    );

    daemon.stop();
}

#[test]
fn project_acceptance_criterion_with_verification_synthesizes_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let verification_id = "verification:v1:project-ac-verification";

    let seed_verification = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-seed-verification",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-seed-verification-key",
            "domain": "verification",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {"records": [verification_record_json(verification_id)]}
        }),
    );
    assert!(
        seed_verification.starts_with("HTTP/1.1 200"),
        "verification fixture should ingest, got {seed_verification}"
    );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-ac-with-verification",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-ac-with-verification-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_external_link_json(PROJECT_EXTERNAL_LINK_ID),
                    project_task_json(PROJECT_TASK_ID, "open", "2026-05-22T00:00:01Z"),
                    project_acceptance_criterion_json(
                        "project:v1:test-ac-with-verification",
                        PROJECT_TASK_ID,
                        "verified",
                        Some(verification_id)
                    )
                ]
            }
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "verified AC with verification target should ingest, got {response}"
    );
    daemon.stop();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    let records = sink
        .read_all_records()
        .expect("read_all_records should succeed");
    for label in [
        EdgeLabel::ExternalHandle,
        EdgeLabel::OwnedByTask,
        EdgeLabel::ClosesAcceptanceCriterion,
    ] {
        assert!(
            records
                .iter()
                .any(|record| matches!(record, GraphRecord::Edge { label: actual, .. } if *actual == label)),
            "project ingest should synthesize {label:?} edge"
        );
    }
}

#[test]
fn project_trust_class_rejects_wrong_domain_and_wrong_verification_target() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let mut wrong_domain_task = project_task_json(
        "project:v1:test-task-wrong-domain",
        "open",
        "2026-05-22T00:00:01Z",
    );
    wrong_domain_task["domain"] = serde_json::Value::String("agent_memory".to_owned());
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-task-wrong-domain",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-task-wrong-domain-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_external_link_json(PROJECT_EXTERNAL_LINK_ID),
                    wrong_domain_task
                ]
            }
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "Task with non-project domain should be rejected, got {response}"
    );

    let file_id = "codegraph:v4:project-ac-not-verification";
    let seed_file = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-seed-codegraph-file",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-seed-codegraph-file-key",
            "domain": "codegraph",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {"records": [codegraph_file_json(file_id)]}
        }),
    );
    assert!(
        seed_file.starts_with("HTTP/1.1 200"),
        "codegraph target fixture should ingest, got {seed_file}"
    );

    let wrong_target = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-ac-wrong-verification-target",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-ac-wrong-verification-target-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_external_link_json(PROJECT_EXTERNAL_LINK_ID),
                    project_task_json(PROJECT_TASK_ID, "open", "2026-05-22T00:00:01Z"),
                    project_acceptance_criterion_json(
                        "project:v1:test-ac-wrong-verification-target",
                        PROJECT_TASK_ID,
                        "verified",
                        Some(file_id)
                    )
                ]
            }
        }),
    );
    assert!(
        wrong_target.starts_with("HTTP/1.1 400"),
        "AC verification_link_id pointing at codegraph should be rejected, got {wrong_target}"
    );

    daemon.stop();
}

#[test]
fn project_task_reimport_preserves_rows_by_transaction_time() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let first = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-task-first-import",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-task-first-import-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_external_link_json(PROJECT_EXTERNAL_LINK_ID),
                    project_task_json(PROJECT_TASK_ID, "open", "2026-05-22T00:00:01Z")
                ]
            }
        }),
    );
    assert!(
        first.starts_with("HTTP/1.1 200"),
        "first task import should succeed, got {first}"
    );

    let second = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "project-task-second-import",
            "agent_id": "project-test-agent",
            "session_id": "project-test-session",
            "idempotency_key": "project-task-second-import-key",
            "domain": "project",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    project_task_json(PROJECT_TASK_ID, "closed_completed", "2026-05-22T00:00:02Z")
                ]
            }
        }),
    );
    assert!(
        second.starts_with("HTTP/1.1 200"),
        "second task import should succeed, got {second}"
    );
    daemon.stop();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    let records = sink
        .read_all_records()
        .expect("read_all_records should succeed");
    let mut task_rows = records
        .iter()
        .filter_map(|record| {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::Task,
                entity_id: Some(entity_id),
                status: Some(status),
                transaction_time: Some(transaction_time),
                ..
            } = record
                && id == PROJECT_TASK_ID
            {
                Some((
                    entity_id.as_str(),
                    status.as_str(),
                    transaction_time.as_str(),
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    task_rows.sort_unstable();

    assert_eq!(
        task_rows,
        vec![
            (
                PROJECT_TASK_ID,
                "closed_completed",
                "2026-05-22T00:00:02Z"
            ),
            (PROJECT_TASK_ID, "open", "2026-05-22T00:00:01Z")
        ],
        "re-importing the same task should preserve both mutation rows with the same entity_id"
    );
}

#[test]
fn agent_actions_schema_doc_is_linked_and_defines_day_one_shapes() {
    let actions = read_repo_text("docs/schema/agent-actions.md");
    for needle in [
        "PatchArtifact -> `artifact` domain",
        "FileEdit -> `agent_memory` domain",
        "ToolCall -> `agent_memory` domain",
        "patch_status",
        "applied_clean",
        "applied_with_conflicts",
        "invalid_syntax",
        "invalid_no_base",
        "rejected_validation",
        "unverified",
        "superseded",
        "PatchArtifact record shape",
        "FileEdit record shape",
        "ToolCall record shape",
        "PatchArtifact.validation_summary",
        "ToolCall.arguments_summary",
        "validity-pinning",
        "SUPERSEDED_BY",
        "schema_version` is `1`",
    ] {
        assert!(
            actions.contains(needle),
            "agent-actions schema must document `{needle}`"
        );
    }

    for path in [
        "README.md",
        "docs/prd/0000-egregore-vision.md",
        "docs/schema/agent-memory.md",
        "docs/schema/verification.md",
        "docs/plans/2026-05-17-egregore-daemon-design.md",
    ] {
        let text = read_repo_text(path);
        assert!(
            text.contains("docs/schema/agent-actions.md") || text.contains("agent-actions.md"),
            "{path} must link to docs/schema/agent-actions.md"
        );
    }
}

#[test]
fn agent_actions_edge_registry_rows_are_documented_with_endpoint_rules() {
    let registry = read_repo_text("docs/schema/agent-memory.md");
    for needle in [
        "| `PRODUCED_PATCH` | `agent_memory` | `artifact` | `FileEdit`, `AgentTurn` | `PatchArtifact` | many:1; FileEdit at most one | no |",
        "| `TOUCHED_FILE` | `agent_memory`, `verification` | `codegraph` | `FileEdit`, `ToolCall`, `CommandRun`, `TestRun`, `CIStatus` | `File` | many:many | no |",
        "| `PRODUCED_EVIDENCE` | `agent_memory` | `verification` | `ToolCall` | `CommandRun`, `TestRun` | many:1 | no |",
    ] {
        assert!(
            registry.contains(needle),
            "agent-memory edge registry must contain exact row: {needle}"
        );
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn patch_artifact_patch_status_is_pinned() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "patch-status-pinned-producer-session-key");

    let first_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "patch-status-pinned-first",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "patch-status-pinned-first-key",
            "domain": "artifact",
            "created_at": "2026-05-21T00:00:00Z",
            "payload": {
                "records": [patch_artifact_fixture(
                    "artifact:v1:patch-status-pinned-fixture",
                    "invalid_syntax",
                    &serde_json::json!({"path": "artifacts/rejected.diff", "inline": "not a diff"}),
                )]
            }
        }),
    );
    assert!(
        first_response.starts_with("HTTP/1.1 200"),
        "initial invalid PatchArtifact should be accepted, got {first_response}"
    );

    let update_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "patch-status-pinned-update",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "patch-status-pinned-update-key",
            "domain": "artifact",
            "created_at": "2026-05-21T00:01:00Z",
            "payload": {
                "records": [patch_artifact_fixture(
                    "artifact:v1:patch-status-pinned-fixture",
                    "applied_clean",
                    &serde_json::json!({"path": "artifacts/repaired.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
                )]
            }
        }),
    );
    assert!(
        !update_response.starts_with("HTTP/1.1 200"),
        "editing PatchArtifact.patch_status should be rejected, got {update_response}"
    );
    let body = response_json(&update_response);
    assert_eq!(
        body["error"]["code"], "patch_status_pinned",
        "patch-status mutation must return patch_status_pinned, got {body}"
    );

    daemon.stop();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn patch_artifact_oversized_inline_patch_handle_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "patch-oversized-inline-producer-session-key");
    let oversized_inline = "x".repeat(17 * 1024);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "patch-oversized-inline",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "patch-oversized-inline-key",
            "domain": "artifact",
            "created_at": "2026-05-21T00:00:00Z",
            "payload": {
                "records": [patch_artifact_fixture(
                    "artifact:v1:oversized-inline-patch-fixture",
                    "unverified",
                    &serde_json::json!({
                        "path": "artifacts/too-large.diff",
                        "inline": oversized_inline
                    }),
                )]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "PatchArtifact.patch_handle.inline over 16 KiB should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "inline_payload_exceeds_ceiling",
        "oversized patch inline payload must reuse inline_payload_exceeds_ceiling, got {body}"
    );

    daemon.stop();
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
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-18T00:00:00Z",
                    "ingested_at": "2026-05-18T00:00:00Z",
                    "confidence": "0.9",
                    "summary": "test observation with unresolved evidence link",
                    "evidence_links": [{
                        "target_record_id": "codegraph:v3:nonexistent-symbol-xyzzy",
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

#[test]
fn daemon_ingest_accepts_current_traj_importer_agent_memory_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let graph = import_traj(
        Path::new("tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj"),
        &ImportOptions::default(),
    )
    .expect("fixture .traj should import");
    let records = graph.records().to_vec();
    let agent_run_ids = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::AgentRun,
                    ..
                }
            )
        })
        .map(|record| record.id().to_owned())
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| matches!(
            record,
            GraphRecord::Node {
                id,
                kind: NodeKind::CommandRun | NodeKind::Verification | NodeKind::PatchArtifact,
                ..
            } if id.starts_with("agent_memory:v1:")
        )),
        ".traj fixture must exercise legacy agent_memory:v1 action/evidence node kinds"
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            GraphRecord::Edge {
                label: EdgeLabel::ProducedPatch,
                source,
                target,
                ..
            } if agent_run_ids.contains(source) && target.starts_with("agent_memory:v1:")
        )),
        ".traj fixture must exercise legacy AgentRun -> agent_memory PatchArtifact edge"
    );
    let records_json = records
        .iter()
        .map(|record| serde_json::to_value(record).expect("record should serialize"))
        .collect::<Vec<_>>();

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "traj-importer-agent-memory-ingest",
            "agent_id": "traj-test-agent",
            "session_id": "traj-test-session",
            "idempotency_key": "traj-importer-agent-memory-ingest-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": records_json }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "current .traj importer output should ingest during migration, got {response}"
    );

    daemon.stop();
}

#[test]
fn produced_patch_evidence_link_requires_artifact_id_target() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let legacy_patch_id = "agent_memory:v1:legacy-patch-evidence-link-target";
    {
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        let legacy_patch = GraphRecord::node(
            legacy_patch_id.to_owned(),
            NodeKind::PatchArtifact,
            None,
            None,
            None,
            "legacy agent-memory PatchArtifact target".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
        sink.write_record(&legacy_patch)
            .expect("legacy patch target should pre-seed");
        sink.persist_indexes()
            .expect("pre-seeded target should persist");
    }
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "produced-patch-evidence-link-artifact-prefix",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "produced-patch-evidence-link-artifact-prefix-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:produced-patch-evidence-link-source",
                    "kind": "AgentTurn",
                    "schema_version": 1,
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "AgentTurn with inconsistent PRODUCED_PATCH evidence link",
                    "evidence_links": [{
                        "target_record_id": legacy_patch_id,
                        "target_domain": "artifact",
                        "relation": "PRODUCED_PATCH",
                        "confidence": "1.0"
                    }]
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "PRODUCED_PATCH evidence links must reject non-artifact targets, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "artifact prefix mismatch should be a bad_request, got {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("artifact:v1:")),
        "artifact prefix mismatch should name artifact:v1:, got {body}"
    );

    daemon.stop();
}

#[test]
fn failed_on_evidence_link_accepts_legacy_agent_memory_patch_target() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let legacy_patch_id = "agent_memory:v1:legacy-patch-failed-on-target";
    {
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        let legacy_patch = GraphRecord::node(
            legacy_patch_id.to_owned(),
            NodeKind::PatchArtifact,
            None,
            None,
            None,
            "legacy agent-memory PatchArtifact failure target".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
        sink.write_record(&legacy_patch)
            .expect("legacy patch target should pre-seed");
        sink.persist_indexes()
            .expect("pre-seeded target should persist");
    }
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "failed-on-legacy-agent-memory-patch",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "failed-on-legacy-agent-memory-patch-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:failed-on-legacy-source",
                    "kind": "Failure",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Failure with legacy patch evidence target",
                    "failure_kind": "patch_invalid",
                    "evidence_links": [{
                        "target_record_id": legacy_patch_id,
                        "target_domain": "agent_memory",
                        "relation": "FAILED_ON",
                        "confidence": "1.0"
                    }]
                }]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "FAILED_ON evidence links should accept legacy agent_memory PatchArtifact targets, got {response}"
    );

    daemon.stop();
}

#[test]
fn legacy_agent_memory_artifact_and_diagnostic_nodes_are_accepted() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "legacy-agent-memory-artifact-diagnostic",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "legacy-agent-memory-artifact-diagnostic-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:legacy-generic-artifact",
                    "kind": "Artifact",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Legacy generic artifact"
                }, {
                    "record_type": "node",
                    "id": "agent_memory:v1:legacy-traj-diagnostic",
                    "kind": "Diagnostic",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Legacy trajectory diagnostic"
                }]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "legacy agent-memory Artifact and Diagnostic nodes should ingest during migration, got {response}"
    );

    daemon.stop();
}

#[test]
fn incomplete_tool_call_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "incomplete-tool-call",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "incomplete-tool-call-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:incomplete-tool-call",
                    "kind": "ToolCall",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Incomplete tool call"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ToolCall missing required agent-actions fields should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_field",
        "incomplete ToolCall should fail with missing_field, got {body}"
    );

    daemon.stop();
}

#[test]
fn incomplete_file_edit_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "incomplete-file-edit",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "incomplete-file-edit-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:incomplete-file-edit",
                    "kind": "FileEdit",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "other",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Incomplete file edit"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "FileEdit missing required agent-actions fields should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_field",
        "incomplete FileEdit should fail with missing_field, got {body}"
    );

    daemon.stop();
}

#[test]
fn file_edit_modify_requires_before_and_after_hashes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-missing-modify-hashes",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-missing-modify-hashes-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:turn-for-file-edit-hash-check",
                    "kind": "AgentTurn",
                    "schema_version": 1,
                    "agent_id": "test-agent",
                    "agent_kind": "codex",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Turn anchoring a FileEdit"
                }, {
                    "record_type": "node",
                    "id": "agent_memory:v1:file-edit-missing-modify-hashes",
                    "kind": "FileEdit",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "codex",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "Modify without hashes",
                    "repo_relative_path": "src/lib.rs",
                    "edit_kind": "modify",
                    "hunk_count": 1,
                    "linked_turn_id": "agent_memory:v1:turn-for-file-edit-hash-check"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "FileEdit modify without before_hash/after_hash should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_field",
        "missing FileEdit hashes should fail with missing_field, got {body}"
    );

    daemon.stop();
}

#[test]
fn linked_turn_id_must_resolve_to_agent_turn() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "linked-turn-id-must-be-turn",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "linked-turn-id-must-be-turn-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "agent_memory:v1:not-a-turn-session",
                    "kind": "AgentSession",
                    "schema_version": 1,
                    "agent_id": "test-agent",
                    "agent_kind": "codex",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "name": "Not a turn",
                    "summary": "Session, not AgentTurn"
                }, {
                    "record_type": "node",
                    "id": "agent_memory:v1:tool-call-linked-to-session",
                    "kind": "ToolCall",
                    "schema_version": 1,
                    "domain": "agent_memory",
                    "agent_id": "test-agent",
                    "agent_kind": "codex",
                    "session_id": "test-session",
                    "observed_at": "2026-05-22T00:00:00Z",
                    "ingested_at": "2026-05-22T00:00:00Z",
                    "summary": "ToolCall linked to wrong node kind",
                    "source_artifact_path": "fixtures/session.traj",
                    "source_artifact_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                    "linked_turn_id": "agent_memory:v1:not-a-turn-session",
                    "tool_name": "Bash",
                    "tool_kind": "bash",
                    "arguments_summary": "cargo test",
                    "arguments_handle": {
                        "hash": "2222222222222222222222222222222222222222222222222222222222222222",
                        "bytes": 10,
                        "inline": "cargo test"
                    },
                    "started_at": "2026-05-22T00:00:00Z",
                    "finished_at": "2026-05-22T00:00:01Z",
                    "status": "succeeded"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "linked_turn_id pointing at AgentSession should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "wrong linked_turn_id kind should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn artifact_domain_record_requires_artifact_id_prefix() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let mut patch = patch_artifact_fixture(
        "agent_memory:v1:artifact-domain-wrong-prefix",
        "unverified",
        &serde_json::json!({"path": "artifacts/legacy.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );
    let patch_obj = patch
        .as_object_mut()
        .expect("patch fixture should be a JSON object");
    patch_obj.insert("agent_id".to_owned(), serde_json::json!("test-agent"));
    patch_obj.insert("agent_kind".to_owned(), serde_json::json!("codex"));
    patch_obj.insert("session_id".to_owned(), serde_json::json!("test-session"));
    patch_obj.insert(
        "observed_at".to_owned(),
        serde_json::json!("2026-05-22T00:00:00Z"),
    );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "artifact-domain-wrong-prefix",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "artifact-domain-wrong-prefix-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [patch] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "artifact-domain record with agent_memory:v1: ID should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "artifact-domain ID prefix mismatch should fail with bad_request, got {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("artifact:v1:")),
        "artifact-domain ID prefix mismatch should mention artifact:v1:, got {body}"
    );

    daemon.stop();
}

#[test]
fn tool_call_produced_evidence_id_must_resolve_to_verification_record() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-produced-evidence-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-missing-produced-evidence", turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .insert(
            "produced_evidence_id".to_owned(),
            serde_json::json!("verification:v1:missing-produced-evidence"),
        );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-missing-produced-evidence",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-missing-produced-evidence-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ToolCall.produced_evidence_id with missing verification target should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "missing produced_evidence_id target should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn file_edit_linked_patch_id_must_resolve_to_patch_artifact() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-linked-patch-check";
    let mut file_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-missing-linked-patch", turn_id);
    file_edit
        .as_object_mut()
        .expect("file edit fixture should be an object")
        .insert(
            "linked_patch_id".to_owned(),
            serde_json::json!("artifact:v1:missing-linked-patch"),
        );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-missing-linked-patch",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-missing-linked-patch-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), file_edit] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "FileEdit.linked_patch_id with missing artifact target should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "missing linked_patch_id target should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn tool_call_result_handle_must_be_valid_when_present() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-result-handle-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-invalid-result-handle", turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .insert(
            "result_handle".to_owned(),
            serde_json::json!({
                "hash": "",
                "bytes": 6,
                "inline": "output"
            }),
        );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-invalid-result-handle",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-invalid-result-handle-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ToolCall.result_handle with empty hash should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "invalid result_handle should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn tool_call_finished_at_must_be_rfc3339_when_present() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-finished-at-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-invalid-finished-at", turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .insert("finished_at".to_owned(), serde_json::json!("not-rfc3339"));

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-invalid-finished-at",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-invalid-finished-at-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ToolCall.finished_at with invalid timestamp should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "invalid finished_at should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn patch_artifact_producer_session_id_must_resolve_to_agent_session() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let mut patch = patch_artifact_fixture(
        "artifact:v1:missing-producer-session-patch",
        "unverified",
        &serde_json::json!({"path": "artifacts/missing-producer.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );
    patch
        .as_object_mut()
        .expect("patch fixture should be an object")
        .insert(
            "producer_session_id".to_owned(),
            serde_json::json!("agent_memory:v1:missing-producer-session"),
        );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "patch-missing-producer-session",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "patch-missing-producer-session-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [patch] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "PatchArtifact.producer_session_id with missing AgentSession should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "missing producer_session_id target should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn patch_artifact_rejects_unknown_base_reason_when_base_commit_is_set() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "patch-known-base-producer-session-key");
    let mut patch = patch_artifact_fixture(
        "artifact:v1:known-base-with-unknown-reason",
        "unverified",
        &serde_json::json!({"path": "artifacts/known-base.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );
    let patch_obj = patch
        .as_object_mut()
        .expect("patch fixture should be an object");
    patch_obj.insert(
        "base_commit".to_owned(),
        serde_json::json!("0123456789abcdef0123456789abcdef01234567"),
    );
    patch_obj.insert(
        "unknown_base_reason".to_owned(),
        serde_json::json!("unknown_base"),
    );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "patch-known-base-with-unknown-reason",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "patch-known-base-with-unknown-reason-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [patch] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "PatchArtifact with base_commit and unknown_base_reason should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "contradictory base provenance should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn invalid_syntax_patch_artifact_requires_empty_target_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "invalid-syntax-target-files-producer-session-key");
    let mut patch = patch_artifact_fixture(
        "artifact:v1:invalid-syntax-with-target-files",
        "invalid_syntax",
        &serde_json::json!({"path": "artifacts/invalid.diff", "inline": "not a diff"}),
    );
    patch
        .as_object_mut()
        .expect("patch fixture should be an object")
        .insert("target_files".to_owned(), serde_json::json!(["src/lib.rs"]));

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "invalid-syntax-target-files",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "invalid-syntax-target-files-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [patch] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "invalid_syntax PatchArtifact with target_files should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "invalid_syntax target_files should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn invalid_no_base_patch_artifact_rejects_base_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "invalid-no-base-producer-session-key");
    let mut patch = patch_artifact_fixture(
        "artifact:v1:invalid-no-base-with-base-commit",
        "invalid_no_base",
        &serde_json::json!({"path": "artifacts/invalid-no-base.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );
    let patch_obj = patch
        .as_object_mut()
        .expect("patch fixture should be an object");
    patch_obj.insert(
        "base_commit".to_owned(),
        serde_json::json!("0123456789abcdef0123456789abcdef01234567"),
    );
    patch_obj.remove("unknown_base_reason");

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "invalid-no-base-with-base-commit",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "invalid-no-base-with-base-commit-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [patch] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "invalid_no_base PatchArtifact with base_commit should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "invalid_no_base base_commit should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn tool_call_and_file_edit_reject_confidence() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let tool_turn_id = "agent_memory:v1:turn-for-tool-confidence-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-with-confidence", tool_turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .insert("confidence".to_owned(), serde_json::json!("0.5"));

    let tool_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-confidence",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-confidence-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(tool_turn_id), tool_call] }
        }),
    );
    assert!(
        !tool_response.starts_with("HTTP/1.1 200"),
        "ToolCall with confidence should be rejected, got {tool_response}"
    );

    let file_turn_id = "agent_memory:v1:turn-for-file-confidence-check";
    let mut file_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-with-confidence", file_turn_id);
    file_edit
        .as_object_mut()
        .expect("file edit fixture should be an object")
        .insert("confidence".to_owned(), serde_json::json!("0.5"));

    let file_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-confidence",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-confidence-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(file_turn_id), file_edit] }
        }),
    );
    assert!(
        !file_response.starts_with("HTTP/1.1 200"),
        "FileEdit with confidence should be rejected, got {file_response}"
    );

    daemon.stop();
}

#[test]
fn tool_call_requires_source_artifact_fields() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-tool-source-artifact-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-missing-source-artifact", turn_id);
    let tool_obj = tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object");
    tool_obj.remove("source_artifact_path");
    tool_obj.remove("source_artifact_hash");

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-missing-source-artifact",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-missing-source-artifact-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "ToolCall without source artifact fields should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_field",
        "missing ToolCall source artifact fields should fail with missing_field, got {body}"
    );

    daemon.stop();
}

#[test]
fn file_edit_requires_source_artifact_fields() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-file-source-artifact-check";
    let mut file_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-missing-source-artifact", turn_id);
    let file_obj = file_edit
        .as_object_mut()
        .expect("file edit fixture should be an object");
    file_obj.remove("source_artifact_path");
    file_obj.remove("source_artifact_hash");

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-missing-source-artifact",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-missing-source-artifact-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), file_edit] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "FileEdit without source artifact fields should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_field",
        "missing FileEdit source artifact fields should fail with missing_field, got {body}"
    );

    daemon.stop();
}

#[test]
fn tool_call_completed_status_requires_finished_at() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-completed-finished-at-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-completed-without-finished-at", turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .remove("finished_at");

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-completed-without-finished-at",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-completed-without-finished-at-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "completed ToolCall without finished_at should be rejected, got {response}"
    );

    daemon.stop();
}

#[test]
fn tool_call_interrupted_status_forbids_finished_at() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:turn-for-interrupted-finished-at-check";
    let mut tool_call =
        valid_tool_call_json("agent_memory:v1:tool-call-interrupted-with-finished-at", turn_id);
    tool_call
        .as_object_mut()
        .expect("tool call fixture should be an object")
        .insert("status".to_owned(), serde_json::json!("interrupted"));

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "tool-call-interrupted-with-finished-at",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "tool-call-interrupted-with-finished-at-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), tool_call] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "interrupted ToolCall with finished_at should be rejected, got {response}"
    );

    daemon.stop();
}

#[test]
fn file_edit_create_and_delete_forbid_opposite_side_hashes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let create_turn_id = "agent_memory:v1:turn-for-create-opposite-hash-check";
    let mut create_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-create-with-before-hash", create_turn_id);
    create_edit
        .as_object_mut()
        .expect("file edit fixture should be an object")
        .insert("edit_kind".to_owned(), serde_json::json!("create"));
    let create_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-create-with-before-hash",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-create-with-before-hash-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(create_turn_id), create_edit] }
        }),
    );
    assert!(
        !create_response.starts_with("HTTP/1.1 200"),
        "FileEdit create with before_hash should be rejected, got {create_response}"
    );

    let delete_turn_id = "agent_memory:v1:turn-for-delete-opposite-hash-check";
    let mut delete_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-delete-with-after-hash", delete_turn_id);
    delete_edit
        .as_object_mut()
        .expect("file edit fixture should be an object")
        .insert("edit_kind".to_owned(), serde_json::json!("delete"));
    let delete_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-delete-with-after-hash",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-delete-with-after-hash-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(delete_turn_id), delete_edit] }
        }),
    );
    assert!(
        !delete_response.starts_with("HTTP/1.1 200"),
        "FileEdit delete with after_hash should be rejected, got {delete_response}"
    );

    daemon.stop();
}

#[test]
fn file_edit_rename_to_forbidden_unless_edit_kind_is_rename() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    let turn_id = "agent_memory:v1:file-edit-rename-to-non-rename-turn";
    let mut file_edit =
        valid_file_edit_json("agent_memory:v1:file-edit-modify-with-rename-to", turn_id);
    file_edit
        .as_object_mut()
        .expect("file edit fixture should be an object")
        .insert("rename_to".to_owned(), serde_json::json!("src/new_lib.rs"));

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "file-edit-rename-to-non-rename",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "file-edit-rename-to-non-rename-key",
            "domain": "agent_memory",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": [agent_turn_json(turn_id), file_edit] }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "FileEdit modify with rename_to should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "rename_to on non-rename FileEdit should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn artifact_patch_records_can_supersede_artifact_patches() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "artifact-supersedes-producer-session-key");
    let prior_patch = patch_artifact_fixture(
        "artifact:v1:prior-patch-to-supersede",
        "invalid_syntax",
        &serde_json::json!({"path": "artifacts/prior.diff", "inline": "not a diff"}),
    );
    let replacement_patch = patch_artifact_fixture(
        "artifact:v1:replacement-patch-supersedes-prior",
        "unverified",
        &serde_json::json!({"path": "artifacts/replacement.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "artifact-patch-supersedes",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "artifact-patch-supersedes-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    prior_patch,
                    replacement_patch,
                    {
                        "record_type": "edge",
                        "id": "artifact:v1:replacement-supersedes-prior-edge",
                        "schema_version": 1,
                        "label": "SUPERSEDES",
                        "source": "artifact:v1:replacement-patch-supersedes-prior",
                        "target": "artifact:v1:prior-patch-to-supersede",
                        "summary": "Replacement patch supersedes prior patch"
                    }
                ]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "artifact-domain PatchArtifact SUPERSEDES edge should be accepted, got {response}"
    );

    daemon.stop();
}

#[test]
fn artifact_domain_edge_rejects_unsupported_labels() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);
    ingest_patch_producer_session(&metadata, "artifact-unsupported-label-producer-session-key");
    let source_patch = patch_artifact_fixture(
        "artifact:v1:unsupported-label-source-patch",
        "invalid_syntax",
        &serde_json::json!({"path": "artifacts/source.diff", "inline": "not a diff"}),
    );
    let target_patch = patch_artifact_fixture(
        "artifact:v1:unsupported-label-target-patch",
        "unverified",
        &serde_json::json!({"path": "artifacts/target.diff", "inline": "diff --git a/src/lib.rs b/src/lib.rs\n"}),
    );

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "artifact-unsupported-label-edge",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "artifact-unsupported-label-edge-key",
            "domain": "artifact",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    source_patch,
                    target_patch,
                    {
                        "record_type": "edge",
                        "id": "artifact:v1:unsupported-contains-edge",
                        "schema_version": 1,
                        "label": "CONTAINS",
                        "source": "artifact:v1:unsupported-label-source-patch",
                        "target": "artifact:v1:unsupported-label-target-patch",
                        "summary": "Unsupported artifact edge label"
                    }
                ]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "artifact-domain edge with unsupported label should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "unsupported artifact edge label should fail with bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_touched_file_evidence_links_accept_verification_sources() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let touched_file_id = "codegraph:v3:verification-touched-file-target";
    {
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        let touched_file = GraphRecord::node(
            touched_file_id.to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            "verification touched file target".to_owned(),
        );
        sink.write_record(&touched_file)
            .expect("touched file target should pre-seed");
        sink.persist_indexes()
            .expect("pre-seeded touched file target should persist");
    }
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "verification-touched-file-link",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "verification-touched-file-link-key",
            "domain": "verification",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    {
                        "record_type": "node",
                        "id": "verification:v1:test-run-touched-file-source",
                        "kind": "TestRun",
                        "schema_version": 1,
                        "domain": "verification",
                        "summary": "TestRun touched file source",
                        "source_artifact_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                        "executed_at": "2026-05-22T00:00:00Z",
                        "evidence_links": [
                            {
                                "relation": "TOUCHED_FILE",
                                "target_domain": "codegraph",
                                "target_record_id": touched_file_id,
                                "confidence": "1.0"
                            }
                        ]
                    }
                ]
            }
        }),
    );

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "verification TestRun TOUCHED_FILE evidence link should be accepted, got {response}"
    );

    daemon.stop();
}

#[test]
fn local_path_repository_identity_rejected_in_shared_store() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let first_repo = GraphRecord::node(
        "codegraph:v3:local-path-repo-first".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repo A".to_owned(),
    )
    .with_repository_identity(RepositoryIdentityPayload {
        identity_source: IdentitySource::LocalRootCommit,
        remote_url: None,
        root_commit_sha: Some("aaaa1111".to_owned()),
        canonical_path: None,
        basename: "repo-a".to_owned(),
    });

    let first_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "local-path-shared-store-first",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "local-path-shared-store-first",
            "domain": "codegraph",
            "created_at": "2026-05-18T00:00:00Z",
            "payload": { "records": [first_repo] }
        }),
    );
    assert!(
        first_response.starts_with("HTTP/1.1 200"),
        "first repo ingest should succeed, got {first_response}"
    );

    let local_path_repo = GraphRecord::node(
        "codegraph:v3:local-path-repo-second".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-b".to_owned()),
        "Repo B".to_owned(),
    )
    .with_repository_identity(RepositoryIdentityPayload {
        identity_source: IdentitySource::LocalPath,
        remote_url: None,
        root_commit_sha: None,
        canonical_path: Some("/tmp/some-path/repo-b".to_owned()),
        basename: "repo-b".to_owned(),
    });

    let second_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "local-path-shared-store-second",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "local-path-shared-store-second",
            "domain": "codegraph",
            "created_at": "2026-05-18T00:00:00Z",
            "payload": { "records": [local_path_repo] }
        }),
    );
    assert!(
        !second_response.starts_with("HTTP/1.1 200"),
        "local_path repository in shared store should be rejected, got {second_response}"
    );

    let body = response_json(&second_response);
    assert_eq!(
        body["error"]["code"], "local_path_identity_unsupported",
        "rejection must carry local_path_identity_unsupported code, got {body}"
    );

    daemon.stop();
}

// ── RED: query verb conformance ───────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn query_verb_conformance() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--repo-id-override")
        .arg("fixture-rust-basic-stable")
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let metadata = read_metadata(&data_dir);

    let records = graph_records_json(&graph_path);
    let ingest_res = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "vqc-ingest",
            "agent_id": "verb-test-agent",
            "session_id": "verb-test-session",
            "idempotency_key": "vqc-ingest-key",
            "domain": "codegraph",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": { "records": records }
        }),
    );
    assert!(
        ingest_res.starts_with("HTTP/1.1 200"),
        "fixture ingest should succeed, got {ingest_res}"
    );

    // ── (e) unknown verb → bad_request with field: "verb" ─────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-unknown-verb",
                "agent_id": "verb-test-agent",
                "verb": "totally_unknown_verb_xyz",
                "params": {}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "unknown verb should return 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false, "error must have ok:false, got {body}");
        assert_eq!(
            body["error"]["code"], "bad_request",
            "unknown verb must return bad_request code, got {body}"
        );
        assert_eq!(
            body["error"]["field"], "verb",
            "unknown verb error must name field:verb, got {body}"
        );
    }

    // ── (b) reserved: observations_for_symbol → not_implemented ───────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-reserved-obs",
                "agent_id": "verb-test-agent",
                "verb": "observations_for_symbol",
                "params": { "symbol_id": "codegraph:v1:fake" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 501"),
            "observations_for_symbol should return 501, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false, "error must have ok:false, got {body}");
        assert_eq!(
            body["error"]["code"], "not_implemented",
            "reserved verb must return not_implemented, got {body}"
        );
    }

    // ── (b) reserved: agent_sessions_for_repo → not_implemented ───────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-reserved-sessions",
                "agent_id": "verb-test-agent",
                "verb": "agent_sessions_for_repo",
                "params": { "repository_id": "some-repo" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 501"),
            "agent_sessions_for_repo should return 501, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(body["ok"], false, "error must have ok:false, got {body}");
        assert_eq!(
            body["error"]["code"], "not_implemented",
            "reserved verb must return not_implemented, got {body}"
        );
    }

    // ── (c) as_of.transaction_time set → not_implemented ──────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-tx-time",
                "agent_id": "verb-test-agent",
                "verb": "symbol_by_name",
                "params": { "name": "Widget" },
                "as_of": { "transaction_time": "2026-01-01T00:00:00Z" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 501"),
            "as_of.transaction_time should return 501, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["error"]["code"], "not_implemented",
            "as_of.transaction_time must return not_implemented, got {body}"
        );
    }

    // ── (d) as_of.since set → not_implemented ─────────────────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-since",
                "agent_id": "verb-test-agent",
                "verb": "symbol_by_name",
                "params": { "name": "Widget" },
                "as_of": { "since": "2026-01-01T00:00:00Z" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 501"),
            "as_of.since should return 501, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["error"]["code"], "not_implemented",
            "as_of.since must return not_implemented, got {body}"
        );
    }

    // ── get_records: existing batch-read behavior preserved ───────────────────
    {
        let first_id = first_record_id(&graph_path);
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-get-records",
                "agent_id": "verb-test-agent",
                "verb": "get_records",
                "params": { "record_ids": [&first_id] }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "get_records should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "get_records must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["verb"], "get_records",
            "result.verb must be get_records, got {body}"
        );
        assert!(
            body["result"].get("snapshot").is_some(),
            "result must include snapshot, got {body}"
        );
        assert!(
            body["result"].get("page").is_some(),
            "result must include page, got {body}"
        );
        assert_eq!(
            body["result"]["page"]["has_more"], false,
            "page.has_more must be false, got {body}"
        );
        let records_arr = body["result"]["records"]
            .as_array()
            .expect("records must be array");
        assert!(
            !records_arr.is_empty(),
            "get_records should return at least one record, got {body}"
        );
        assert!(
            res.contains(&first_id),
            "get_records response must contain the requested record_id"
        );
    }

    // ── symbol_by_name: finds nested::Widget ──────────────────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-symbol-by-name",
                "agent_id": "verb-test-agent",
                "verb": "symbol_by_name",
                "params": { "name": "nested::Widget" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "symbol_by_name should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "symbol_by_name must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["verb"], "symbol_by_name",
            "result.verb must be symbol_by_name, got {body}"
        );
        let records_arr = body["result"]["records"]
            .as_array()
            .expect("records must be array");
        assert!(
            !records_arr.is_empty(),
            "symbol_by_name(nested::Widget) must return at least one record"
        );
        for r in records_arr {
            assert!(
                r.get("record_id").is_some(),
                "record must have record_id, got {r}"
            );
            assert_eq!(
                r["name"], "nested::Widget",
                "record must have name=nested::Widget, got {r}"
            );
            assert_eq!(r["kind"], "Symbol", "record must have kind=Symbol, got {r}");
        }
    }

    // ── symbol_at_commit: no history in current-tree fixture → empty ──────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-symbol-at-commit",
                "agent_id": "verb-test-agent",
                "verb": "symbol_at_commit",
                "params": { "name": "nested::Widget", "commit": "abc123dummy" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "symbol_at_commit should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "symbol_at_commit must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["verb"], "symbol_at_commit",
            "result.verb must be symbol_at_commit, got {body}"
        );
        let records_arr = body["result"]["records"]
            .as_array()
            .expect("records must be array");
        // Current-tree fixture has no temporal metadata, so commit query returns empty
        assert_eq!(
            records_arr.len(),
            0,
            "symbol_at_commit on non-history fixture should return empty, got {body}"
        );
    }

    // ── file_defines: lists symbols in src/lib.rs ──────────────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-file-defines",
                "agent_id": "verb-test-agent",
                "verb": "file_defines",
                "params": { "repo_relative_path": "src/lib.rs" }
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "file_defines should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "file_defines must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["verb"], "file_defines",
            "result.verb must be file_defines, got {body}"
        );
        let records_arr = body["result"]["records"]
            .as_array()
            .expect("records must be array");
        assert!(
            !records_arr.is_empty(),
            "file_defines(src/lib.rs) must return at least one symbol"
        );
        for r in records_arr {
            assert!(
                r.get("record_id").is_some(),
                "record must have record_id, got {r}"
            );
            assert_eq!(
                r["kind"], "Symbol",
                "file_defines must return Symbol records, got {r}"
            );
        }
    }

    // ── drift_top_n: empty for current-tree fixture ───────────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-drift-top-n",
                "agent_id": "verb-test-agent",
                "verb": "drift_top_n",
                "params": {}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "drift_top_n should return 200, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["ok"], true,
            "drift_top_n must have ok:true, got {body}"
        );
        assert_eq!(
            body["result"]["verb"], "drift_top_n",
            "result.verb must be drift_top_n, got {body}"
        );
        // Current-tree fixture has no SemanticDrift records
        let records_arr = body["result"]["records"]
            .as_array()
            .expect("records must be array");
        assert_eq!(
            records_arr.len(),
            0,
            "fixture has no drift records, expected empty list, got {body}"
        );
    }

    // ── response envelope: snapshot is RFC3339, page has correct shape ─────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-envelope-check",
                "agent_id": "verb-test-agent",
                "verb": "get_records",
                "params": { "record_ids": [] }
            }),
        );
        let body = response_json(&res);
        let snapshot = body["result"]["snapshot"]
            .as_str()
            .expect("snapshot must be a string");
        assert!(
            chrono::DateTime::parse_from_rfc3339(snapshot).is_ok(),
            "snapshot must be a valid RFC3339 instant, got '{snapshot}'"
        );
        let page = &body["result"]["page"];
        assert!(
            page.get("has_more").is_some(),
            "page must include has_more, got {page}"
        );
        assert!(
            page.get("returned").is_some(),
            "page must include returned, got {page}"
        );
        assert_eq!(
            page["has_more"], false,
            "page.has_more must be false for empty result, got {page}"
        );
        assert_eq!(
            page["returned"], 0,
            "page.returned must be 0 for empty result, got {page}"
        );
    }

    // ── missing verb → missing_field ──────────────────────────────────────────
    {
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-missing-verb",
                "agent_id": "verb-test-agent",
                "params": {}
            }),
        );
        assert!(
            res.starts_with("HTTP/1.1 400"),
            "missing verb should return 400, got {res}"
        );
        let body = response_json(&res);
        assert_eq!(
            body["error"]["code"], "missing_field",
            "missing verb must return missing_field code, got {body}"
        );
        assert_eq!(
            body["error"]["field"], "verb",
            "missing verb error must name field:verb, got {body}"
        );
    }

    // ── parity: symbol_by_name daemon matches eg query symbol JSONL output ─────
    {
        // Run CLI to get JSONL output for nested::Widget
        let cli_output = Command::cargo_bin("egregore")
            .expect("binary should run")
            .arg("query")
            .arg("symbol")
            .arg("nested::Widget")
            .arg("--graph")
            .arg(&graph_path)
            .output()
            .expect("CLI query should succeed");
        let cli_lines: Vec<serde_json::Value> = String::from_utf8_lossy(&cli_output.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("CLI output must be valid JSON"))
            .collect();

        // Query daemon
        let res = http_json(
            &metadata,
            "POST",
            "/v1/query",
            &serde_json::json!({
                "request_id": "vqc-parity-symbol",
                "agent_id": "verb-test-agent",
                "verb": "symbol_by_name",
                "params": { "name": "nested::Widget" }
            }),
        );
        let body = response_json(&res);
        let daemon_records = body["result"]["records"]
            .as_array()
            .expect("records must be array");

        // Both sides must be non-empty (the fixture always has nested::Widget)
        assert!(
            !cli_lines.is_empty(),
            "CLI query symbol nested::Widget must return at least one record"
        );
        // Compare record_ids (parity check)
        let mut cli_ids: Vec<&str> = cli_lines
            .iter()
            .filter_map(|v| v["record_id"].as_str())
            .collect();
        let mut daemon_ids: Vec<&str> = daemon_records
            .iter()
            .filter_map(|v| v["record_id"].as_str())
            .collect();
        cli_ids.sort_unstable();
        daemon_ids.sort_unstable();
        assert_eq!(
            cli_ids, daemon_ids,
            "daemon symbol_by_name record_ids must match CLI eg query symbol output"
        );
    }

    daemon.stop();
}

#[test]
fn eg_query_daemon_smoke() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let graph_path = temp.path().join("graph.jsonl");
    let mut daemon = start_daemon(&data_dir);

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--repo-id-override")
        .arg("fixture-rust-basic-stable")
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let metadata = read_metadata(&data_dir);
    let records = graph_records_json(&graph_path);
    let ingest_res = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "smoke-ingest",
            "agent_id": "smoke-agent",
            "session_id": "smoke-session",
            "idempotency_key": "smoke-ingest-key",
            "domain": "codegraph",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": { "records": records }
        }),
    );
    assert!(
        ingest_res.starts_with("HTTP/1.1 200"),
        "smoke ingest should succeed, got {ingest_res}"
    );

    // eg query symbol nested::Widget --daemon --data-dir <dir>
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("symbol")
        .arg("nested::Widget")
        .arg("--daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("nested::Widget"))
        .stdout(predicate::str::contains("Symbol"));

    // eg query file src/lib.rs --daemon --data-dir <dir>
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("file")
        .arg("src/lib.rs")
        .arg("--daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("Symbol"));

    // eg query drift --daemon --data-dir <dir> → no drift records → exit 2
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("drift")
        .arg("--daemon")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("no match found"));

    daemon.stop();
}

#[test]
fn semantic_drift_rejects_non_codegraph_prior_target() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    ingest_semantic_codegraph_targets(&metadata, "semantic-bad-prior-targets");

    let drift_id = "semantic:v1:bad-prior-fixture";
    let target_id = "codegraph:v4:semantic-target-file";
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "semantic-bad-prior",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "semantic-bad-prior-key",
            "domain": "semantic",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    semantic_drift_json(drift_id, target_id, "agent_memory:v1:not-codegraph-prior", 0.75),
                    semantic_edge_json("semantic:v1:bad-prior-from", "DRIFTS_FROM", drift_id, target_id),
                    semantic_edge_json("semantic:v1:bad-prior-prior", "DRIFTS_PRIOR", drift_id, "agent_memory:v1:not-codegraph-prior")
                ]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "non-codegraph semantic prior should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "drift_prior_target_mismatch",
        "bad prior target must use drift_prior_target_mismatch, got {body}"
    );

    daemon.stop();
}

#[test]
fn semantic_drift_records_are_immutable_at_stable_id() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    ingest_semantic_codegraph_targets(&metadata, "semantic-immutable-targets");

    let drift_id = "semantic:v1:immutable-drift-fixture";
    let target_id = "codegraph:v4:semantic-target-file";
    let prior_id = "codegraph:v4:semantic-prior-file";
    let valid_records = serde_json::json!([
        semantic_drift_json(drift_id, target_id, prior_id, 0.75),
        semantic_edge_json("semantic:v1:immutable-from", "DRIFTS_FROM", drift_id, target_id),
        semantic_edge_json("semantic:v1:immutable-prior", "DRIFTS_PRIOR", drift_id, prior_id)
    ]);
    let first = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "semantic-immutable-first",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "semantic-immutable-first-key",
            "domain": "semantic",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": { "records": valid_records }
        }),
    );
    assert!(
        first.starts_with("HTTP/1.1 200"),
        "valid semantic drift ingest should succeed, got {first}"
    );

    let mutation = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "semantic-immutable-mutation",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "semantic-immutable-mutation-key",
            "domain": "semantic",
            "created_at": "2026-05-22T00:01:00Z",
            "payload": {
                "records": [
                    semantic_drift_json(drift_id, target_id, prior_id, 0.9)
                ]
            }
        }),
    );
    assert!(
        !mutation.starts_with("HTTP/1.1 200"),
        "mutating a same-ID semantic drift score should be rejected, got {mutation}"
    );
    let body = response_json(&mutation);
    assert_eq!(
        body["error"]["code"], "drift_record_immutable",
        "same-ID score mutation must use drift_record_immutable, got {body}"
    );

    daemon.stop();
}

fn ingest_semantic_codegraph_targets(metadata: &DaemonMetadata, key_suffix: &str) {
    let response = http_json(
        metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": format!("{key_suffix}-codegraph"),
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": format!("{key_suffix}-codegraph-key"),
            "domain": "codegraph",
            "created_at": "2026-05-22T00:00:00Z",
            "payload": {
                "records": [
                    codegraph_file_json("codegraph:v4:semantic-prior-file"),
                    codegraph_file_json("codegraph:v4:semantic-target-file")
                ]
            }
        }),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "codegraph target fixture ingest should succeed, got {response}"
    );
}

fn semantic_drift_json(
    id: &str,
    target_record_id: &str,
    prior_record_id: &str,
    score: f64,
) -> serde_json::Value {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "SemanticDrift",
        "schema_version": SEMANTIC_SCHEMA_VERSION,
        "repo_relative_path": "src/lib.rs",
        "name": "src/lib.rs",
        "summary": "semantic drift fixture",
        "domain": "semantic",
        "valid_time": "2026-05-22T00:00:00Z",
        "valid_time_source": "after_valid_time",
        "ingested_at": "2026-05-22T00:00:01Z",
        "semantic_drift": {
            "embedding_model": {
                "provider": "test",
                "name": "fixture-model",
                "version": "v1",
                "dim": 384,
                "content_hash": "fixture-hash"
            },
            "target_record_id": target_record_id,
            "prior_record_id": prior_record_id,
            "before_git_commit": "aaaaaaaa",
            "after_git_commit": "bbbbbbbb",
            "before_valid_time": "2026-05-21T00:00:00Z",
            "after_valid_time": "2026-05-22T00:00:00Z",
            "metric_kind": "cosine_distance",
            "score": score,
            "selection_threshold": 0.7,
            "selection_basis": "threshold_only"
        }
    })
}

fn semantic_edge_json(
    id: &str,
    label: &str,
    source: &str,
    target: &str,
) -> serde_json::Value {
    serde_json::json!({
        "record_type": "edge",
        "id": id,
        "schema_version": SEMANTIC_SCHEMA_VERSION,
        "label": label,
        "source": source,
        "target": target,
        "confidence": "1.0",
        "summary": "semantic drift edge fixture"
    })
}

// ── Schema conformance: issue #11 (verification domain) ──────────────────────

// RED: (d) A Verification record without an evidence handle is rejected
// with the documented `missing_evidence_handle` error code.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn verification_missing_evidence_handle_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-missing-evidence-handle",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-missing-evidence-handle-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:no-evidence-handle-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with no evidence handle",
                    "executed_at": "2026-05-19T00:00:00Z",
                    "ingested_at": "2026-05-19T00:00:00Z",
                    "verification_kind": "command_run",
                    "status": "passed",
                    "evidence_quality": "verbatim"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "Verification record without evidence handle should be rejected, got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_evidence_handle",
        "rejection must carry missing_evidence_handle code per schema doc, got {body}"
    );

    daemon.stop();
}

// RED: (c) A CommandRun with stdout_handle.inline set and bytes > 16 KiB
// is rejected; accepted when correctly demoted to handle-only (inline=null).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn verification_command_run_oversized_inline_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // 17 KiB of inline content — exceeds the 16 KiB ceiling
    let oversized_inline: String = "x".repeat(17 * 1024);

    let reject_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-oversized-inline-reject",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-oversized-inline-reject-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:oversized-inline-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with oversized inline stdout",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "executed_at": "2026-05-19T00:00:00Z",
                    "ingested_at": "2026-05-19T00:00:00Z",
                    "evidence_quality": "verbatim",
                    "stdout_handle": {
                        "inline": oversized_inline,
                        "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                        "bytes": 17408_u64
                    }
                }]
            }
        }),
    );

    assert!(
        !reject_response.starts_with("HTTP/1.1 200"),
        "CommandRun with oversized stdout_handle.inline should be rejected, got {reject_response}"
    );
    let reject_body = response_json(&reject_response);
    assert_eq!(
        reject_body["error"]["code"], "inline_payload_exceeds_ceiling",
        "oversized inline stdout rejection must carry inline_payload_exceeds_ceiling, got {reject_body}"
    );

    // Accept when inline demoted to null (handle-only)
    let accept_response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-oversized-inline-accept",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-oversized-inline-accept-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:handle-only-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with handle-only stdout (demoted)",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "executed_at": "2026-05-19T00:00:00Z",
                    "ingested_at": "2026-05-19T00:00:00Z",
                    "evidence_quality": "referenced_only",
                    "stdout_handle": {
                        "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                        "bytes": 17408_u64
                    }
                }]
            }
        }),
    );

    assert!(
        accept_response.starts_with("HTTP/1.1 200"),
        "CommandRun with handle-only stdout (inline=null) should be accepted, got {accept_response}"
    );

    daemon.stop();
}

#[test]
fn verification_empty_artifact_hash_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // source_artifact_hash present but empty — must be treated as missing
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-empty-hash-reject",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-empty-hash-reject-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:empty-hash-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with empty source_artifact_hash - should be rejected",
                    "source_artifact_hash": ""
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "empty source_artifact_hash should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "missing_evidence_handle",
        "empty hash must produce missing_evidence_handle, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_wrong_schema_version_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-wrong-schema-version",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-wrong-schema-version-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:wrong-schema-version-fixture",
                    "kind": "TestRun",
                    "schema_version": 2,
                    "summary": "TestRun with unsupported schema_version",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "unsupported schema_version should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "unknown_schema_version",
        "wrong schema_version must produce unknown_schema_version, got {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("verification TestRun v2")),
        "unknown schema-version error must surface the tuple, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_spoofed_bytes_oversized_inline_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // 17 KiB inline but bytes field claims only 100 bytes — should still be rejected
    let oversized_inline: String = "x".repeat(17 * 1024);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-spoofed-bytes-reject",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-spoofed-bytes-reject-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:spoofed-bytes-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with spoofed bytes field",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "stdout_handle": {
                        "inline": oversized_inline,
                        "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                        "bytes": 100_u64
                    }
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "spoofed bytes with oversized inline should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "spoofed bytes rejection must carry bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_wrong_node_kind_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // NodeKind::File is not a verification kind
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-wrong-kind",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-wrong-kind-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:wrong-kind-fixture",
                    "kind": "File",
                    "schema_version": 1,
                    "summary": "File node under verification domain - should be rejected",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "non-verification kind under verification domain should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "wrong kind must produce bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_invalid_executed_at_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-bad-executed-at",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-bad-executed-at-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:bad-executed-at-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with malformed executed_at",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "executed_at": "not-a-timestamp"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "invalid executed_at should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "invalid executed_at must produce bad_request, got {body}"
    );

    daemon.stop();
}

#[test]
fn verification_v2_id_prefix_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // verification:v2: does not match the accepted v1 prefix
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-v2-prefix",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-v2-prefix-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v2:some-future-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with v2 ID prefix - should be rejected",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "verification:v2: prefix should be rejected; got {response}"
    );

    daemon.stop();
}

#[test]
fn verification_stdout_handle_empty_hash_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let mut daemon = start_daemon(&data_dir);
    let metadata = read_metadata(&data_dir);

    // stdout_handle present with empty hash — must be rejected even when source_artifact_hash is valid
    let response = http_json(
        &metadata,
        "POST",
        "/v1/records/ingest",
        &serde_json::json!({
            "request_id": "ver-empty-stdout-hash",
            "agent_id": "test-agent",
            "session_id": "test-session",
            "idempotency_key": "ver-empty-stdout-hash-key",
            "domain": "verification",
            "created_at": "2026-05-19T00:00:00Z",
            "payload": {
                "records": [{
                    "record_type": "node",
                    "id": "verification:v1:empty-stdout-hash-fixture",
                    "kind": "TestRun",
                    "schema_version": 1,
                    "summary": "TestRun with empty stdout_handle.hash",
                    "source_artifact_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "stdout_handle": {
                        "hash": "",
                        "bytes": 100_u64
                    }
                }]
            }
        }),
    );

    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "stdout_handle with empty hash should be rejected; got {response}"
    );
    let body = response_json(&response);
    assert_eq!(
        body["error"]["code"], "bad_request",
        "empty stdout_handle.hash must produce bad_request, got {body}"
    );

    daemon.stop();
}
