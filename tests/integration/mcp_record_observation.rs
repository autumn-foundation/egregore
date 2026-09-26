//! MCP `record_observation` write tool — issue #183.
//!
//! The tool closes the agent learning loop over the MCP transport: an agent
//! records an evidence-backed observation into the `agent_memory` domain with
//! the same provenance contract the CLI enforces (shared
//! `validate_observation_request`), stamped with an MCP-originated producer
//! envelope (`transport: "mcp"`). Malformed calls are rejected with the
//! structured `{"ok": false, "error": ...}` envelope and never touch the
//! store. The ingest domain is hardcoded to `agent_memory`
//! (`DaemonClient::ingest_agent_memory_records`); the tool has no domain
//! parameter, so observations can never land in the deterministic `codegraph`
//! domain.

#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    daemon::{DaemonClient, runtime_dir_for_data_dir},
    evidence::{
        EvidenceProvenance, ObservationRequest, build_observation_records,
        build_observation_records_with_producer, mcp_observation_producer,
    },
    ir::{GraphRecord, NodeKind, ProducerKind},
    mcp::{
        EgregoreMcpServer, RecordObservationArgs, RecordObservationEvidenceTarget,
        record_observation_request_from_args,
    },
    mcp_contract::{MCP_CONTRACT_TOOLS, error_schema, response_schema},
};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// Well-formed but store-absent codegraph ID: cites nothing live.
const DANGLING_SYMBOL_ID: &str =
    "codegraph:v11:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

fn evidence_target(target_record_id: &str, target_domain: &str) -> RecordObservationEvidenceTarget {
    RecordObservationEvidenceTarget {
        target_record_id: Some(target_record_id.to_owned()),
        target_domain: Some(target_domain.to_owned()),
    }
}

/// Fully valid tool arguments. `data_dir` and the evidence target are filled
/// by the caller.
fn valid_args(
    data_dir: &Path,
    target_record_id: &str,
    target_domain: &str,
) -> RecordObservationArgs {
    RecordObservationArgs {
        agent_id: Some("test-agent".to_owned()),
        agent_kind: Some("other".to_owned()),
        session_id: Some("sess-001".to_owned()),
        observed_at: Some("2026-09-26T16:00:00Z".to_owned()),
        source_handle: Some("src/lib.rs".to_owned()),
        text: Some("Widget::new returns early on zero capacity".to_owned()),
        confidence: Some(0.85),
        evidence: Some(vec![evidence_target(target_record_id, target_domain)]),
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
    }
}

fn valid_request() -> ObservationRequest {
    let target = evidence_target(DANGLING_SYMBOL_ID, "codegraph");
    let args = RecordObservationArgs {
        agent_id: Some("test-agent".to_owned()),
        agent_kind: Some("other".to_owned()),
        session_id: Some("sess-001".to_owned()),
        observed_at: Some("2026-09-26T16:00:00Z".to_owned()),
        source_handle: Some("src/lib.rs".to_owned()),
        text: Some("well-formed".to_owned()),
        confidence: Some(0.85),
        evidence: Some(vec![target]),
        data_dir: Some("/tmp/unused".to_owned()),
    };
    record_observation_request_from_args(&args).expect("valid args must build a request")
}

// ── Contract: tool registration ───────────────────────────────────────────────

#[test]
fn tool_is_registered_in_contract_tool_list() {
    assert!(
        MCP_CONTRACT_TOOLS.contains(&"record_observation"),
        "record_observation must be in MCP_CONTRACT_TOOLS"
    );
    // Contract stays at version 1: adding a tool is additive, not breaking.
    assert_eq!(
        aletheia_egregore::mcp_contract::MCP_CONTRACT_VERSION,
        1,
        "MCP contract must stay at version 1"
    );
}

#[test]
fn tool_has_published_response_schema() {
    let schema =
        response_schema("record_observation").expect("record_observation must have a schema");
    let validator =
        jsonschema::validator_for(&schema).expect("published schema must be a valid JSON Schema");
    let ok_instance = serde_json::json!({
        "ok": true,
        "record_id": "agent_memory:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "records_written": 5,
    });
    assert!(
        validator.is_valid(&ok_instance),
        "canonical success payload must validate against the published schema"
    );
}

#[test]
fn tool_response_schema_pins_citable_record_id() {
    let schema =
        response_schema("record_observation").expect("record_observation must have a schema");
    let required = schema["required"]
        .as_array()
        .expect("schema must declare required fields");
    assert!(
        required.iter().any(|v| v.as_str() == Some("record_id")),
        "success response must require record_id, got {schema}"
    );
    assert!(
        required.iter().any(|v| v.as_str() == Some("ok")),
        "success response must require ok, got {schema}"
    );
}

// ── Pure validation: missing / invalid provenance ─────────────────────────────

/// (fixture description, mutator) pairs — every one must be rejected
/// structurally and must write nothing.
type ArgsMutator = Box<dyn Fn(&mut RecordObservationArgs)>;

fn malformed_fixtures() -> Vec<(&'static str, ArgsMutator)> {
    vec![
        ("missing session_id", Box::new(|a| a.session_id = None)),
        (
            "empty session_id",
            Box::new(|a| {
                a.session_id = Some(String::new());
            }),
        ),
        (
            "missing text",
            Box::new(|a| {
                a.text = None;
            }),
        ),
        (
            "empty text",
            Box::new(|a| {
                a.text = Some(String::new());
            }),
        ),
        (
            "missing confidence",
            Box::new(|a| {
                a.confidence = None;
            }),
        ),
        (
            "confidence out of range",
            Box::new(|a| {
                a.confidence = Some(1.5);
            }),
        ),
        (
            "negative confidence",
            Box::new(|a| {
                a.confidence = Some(-0.1);
            }),
        ),
        (
            "missing agent_id",
            Box::new(|a| {
                a.agent_id = None;
            }),
        ),
        (
            "missing source_handle",
            Box::new(|a| {
                a.source_handle = None;
            }),
        ),
        (
            "malformed observed_at",
            Box::new(|a| {
                a.observed_at = Some("not-a-timestamp".to_owned());
            }),
        ),
        (
            "invalid agent_kind",
            Box::new(|a| {
                a.agent_kind = Some("skynet".to_owned());
            }),
        ),
        (
            "missing evidence",
            Box::new(|a| {
                a.evidence = None;
            }),
        ),
        (
            "empty evidence list",
            Box::new(|a| {
                a.evidence = Some(vec![]);
            }),
        ),
        (
            "evidence target without record id",
            Box::new(|a| {
                a.evidence = Some(vec![RecordObservationEvidenceTarget {
                    target_record_id: None,
                    target_domain: Some("codegraph".to_owned()),
                }]);
            }),
        ),
        (
            "evidence target without domain",
            Box::new(|a| {
                a.evidence = Some(vec![RecordObservationEvidenceTarget {
                    target_record_id: Some(DANGLING_SYMBOL_ID.to_owned()),
                    target_domain: None,
                }]);
            }),
        ),
        (
            "evidence target with forbidden domain",
            Box::new(|a| {
                a.evidence = Some(vec![evidence_target(DANGLING_SYMBOL_ID, "agent_memory")]);
            }),
        ),
    ]
}

#[test]
fn malformed_calls_are_rejected_structurally() {
    let err_schema = error_schema();
    let validator =
        jsonschema::validator_for(&err_schema).expect("error schema must be a valid JSON Schema");
    for (name, mutate) in malformed_fixtures() {
        let mut args = valid_args(Path::new("/tmp/unused"), DANGLING_SYMBOL_ID, "codegraph");
        mutate(&mut args);
        let error = record_observation_request_from_args(&args)
            .err()
            .unwrap_or_else(|| panic!("{name}: malformed call must be rejected"));
        let payload = aletheia_egregore::mcp::provenance_error_payload(&error);
        assert_eq!(
            payload["ok"],
            Value::from(false),
            "{name}: envelope must carry ok:false"
        );
        let code = payload["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: error must carry a code"));
        assert!(
            code == "missing_field" || code == "invalid_field",
            "{name}: error code must be a frozen code, got {code}"
        );
        let errors: Vec<String> = validator
            .iter_errors(&payload)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{name}: error must validate against the published error schema:\n  - {}\npayload: {payload}",
            errors.join("\n  - ")
        );
    }
}

#[test]
fn malformed_calls_never_echo_payload_text() {
    let secret = "the widget constructor panics on zero capacity";
    for (name, mutate) in malformed_fixtures() {
        let mut args = valid_args(Path::new("/tmp/unused"), DANGLING_SYMBOL_ID, "codegraph");
        args.text = Some(secret.to_owned());
        mutate(&mut args);
        if let Err(error) = record_observation_request_from_args(&args) {
            let payload = aletheia_egregore::mcp::provenance_error_payload(&error);
            let rendered = payload.to_string();
            assert!(
                !rendered.contains(secret),
                "{name}: structured errors must not echo payload text"
            );
        }
    }
}

#[test]
fn forbidden_evidence_domain_is_rejected_before_daemon_contact() {
    // The CLI rejects non-codegraph/non-verification evidence domains before
    // building records; the MCP tool must reject them in its pure validation
    // layer too, so no daemon contact is needed for the rejection.
    let args = valid_args(Path::new("/tmp/unused"), DANGLING_SYMBOL_ID, "agent_memory");
    let error = record_observation_request_from_args(&args)
        .err()
        .expect("agent_memory evidence domain must be rejected");
    assert_eq!(error.code, "invalid_field");
    assert_eq!(error.field, "evidence.target_domain");
}

// ── Producer envelope: MCP origin distinguishable from CLI origin ─────────────

#[test]
fn mcp_producer_is_distinguishable_from_cli_producer() {
    let req = valid_request();
    let cli = build_observation_records(&req).expect("CLI builder must succeed");
    let mcp = build_observation_records_with_producer(
        &req,
        &mcp_observation_producer(&req.provenance.observed_at),
    )
    .expect("MCP builder must succeed");

    // Same request, same transport-independent IDs: the producer envelope
    // never participates in stable ID composition.
    assert_eq!(
        cli.record_id, mcp.record_id,
        "record_id must be transport-independent"
    );

    for outcome in [&cli, &mcp] {
        for record in &outcome.records {
            match record {
                GraphRecord::Node { producer, .. }
                | GraphRecord::Edge { producer, .. }
                | GraphRecord::Tombstone { producer, .. } => {
                    let producer = producer
                        .as_ref()
                        .expect("observation records must carry a producer envelope");
                    assert_eq!(
                        producer.producer_kind,
                        ProducerKind::ObservationWriter,
                        "both transports stamp ObservationWriter"
                    );
                }
            }
        }
    }

    let mcp_transport = |outcome: &aletheia_egregore::evidence::EvidenceWriteOutcome| {
        outcome
            .records
            .iter()
            .map(|record| match record {
                GraphRecord::Node { producer, .. }
                | GraphRecord::Edge { producer, .. }
                | GraphRecord::Tombstone { producer, .. } => producer
                    .as_ref()
                    .and_then(|p| p.producer_components.get("transport").cloned()),
            })
            .collect::<Vec<_>>()
    };
    assert!(
        mcp_transport(&mcp)
            .iter()
            .all(|t| t.as_deref() == Some("mcp")),
        "every MCP record must stamp transport=mcp"
    );
    assert!(
        mcp_transport(&cli).iter().all(Option::is_none),
        "CLI records must not carry a transport marker"
    );
}

// ── Daemon round trip ─────────────────────────────────────────────────────────

struct RunningDaemon {
    child: Option<Child>,
    data_dir: PathBuf,
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
                .arg("daemon")
                .arg("stop")
                .arg("--data-dir")
                .arg(&self.data_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn start_daemon(data_dir: &Path) -> RunningDaemon {
    fs::create_dir_all(data_dir).expect("should create data dir");
    let stdout_file = fs::File::create(data_dir.join("daemon.stdout")).expect("stdout file");
    let stderr_file = fs::File::create(data_dir.join("daemon.stderr")).expect("stderr file");
    let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"));
    command
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdout(stdout_file)
        .stderr(stderr_file);
    let child = command.spawn().expect("daemon should spawn");
    // Use the published runtime-dir helper (not a reimplementation of its
    // naming) and wait for the running state, mirroring
    // tests/integration/daemon.rs.
    let metadata_path = runtime_dir_for_data_dir(data_dir).join("egregored.json");
    let start = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(&metadata_path)
            && let Ok(metadata) = serde_json::from_str::<Value>(&contents)
            && metadata["state"] == "running"
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "daemon metadata should reach running at {}",
            metadata_path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
    RunningDaemon {
        child: Some(child),
        data_dir: data_dir.to_path_buf(),
    }
}

/// Seed one deterministic codegraph symbol via the CLI scan + daemon ingest
/// path (the same shape production data takes), then return the symbol's
/// record ID for use as the evidence target.
fn seed_widget_symbol(data_dir: &Path) -> String {
    let temp = data_dir
        .parent()
        .expect("data dir must have a parent")
        .to_path_buf();
    let graph_path = temp.join("graph.jsonl");
    let fixture_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic");

    // The scan pins a stable repository identity via `--repo-id-override`: the
    // daemon's shared-store rule blocks agent-memory writes to stores that
    // contain a local-path repository (identity source `local_path`), so the
    // seed must not leave the store local-path-locked — exactly as the
    // project's own daemon tests do with `fixture-rust-basic-stable`.
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&fixture_repo)
        .arg("--repo-id-override")
        .arg("mcp-record-observation-widget-repo")
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("daemon")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--agent-id")
        .arg("test-agent")
        .arg("--session-id")
        .arg("sess-000")
        .arg("--idempotency-key")
        .arg("fixture-ingest")
        .assert()
        .success();

    let client = DaemonClient::from_data_dir(data_dir).expect("daemon client should connect");
    // The fixture declares `pub struct Widget` inside `pub mod nested`; the
    // stored symbol name is the qualified `nested::Widget`.
    let records = client
        .query_verb(
            "symbol_by_name",
            &serde_json::json!({ "name": "nested::Widget" }),
            None,
        )
        .expect("symbol_by_name must succeed");
    records
        .first()
        .and_then(|record| record["record_id"].as_str())
        .expect("scan must produce a nested::Widget symbol")
        .to_owned()
}

/// Sorted record IDs currently live in the store — the "byte-for-byte
/// unchanged" oracle for rejection tests.
fn live_record_ids(client: &DaemonClient) -> Vec<String> {
    let (records, _, _) = client
        .get_all_records()
        .expect("get_all_records must succeed");
    let mut ids: Vec<String> = records.iter().map(|r| r.id().to_owned()).collect();
    ids.sort_unstable();
    ids
}

fn assert_valid(schema: &Value, instance: &Value, what: &str) {
    let validator =
        jsonschema::validator_for(schema).expect("published schema must be a valid JSON Schema");
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{what} failed schema validation:\n  - {}\ninstance: {instance}",
        errors.join("\n  - ")
    );
}

/// Full loop with zero MCP-internal shortcuts: scan + ingest a codegraph
/// Asserts the record named by `record_id` is a persisted Observation node
/// with the MCP-origin producer envelope and an evidence link to the seed
/// symbol.
fn assert_persisted_mcp_observation(records: &[GraphRecord], record_id: &str, widget_id: &str) {
    let observation = records
        .iter()
        .find(|r| r.id() == record_id)
        .expect("written observation must be readable back");
    match observation {
        GraphRecord::Node {
            kind,
            producer,
            evidence_links,
            ..
        } => {
            assert_eq!(*kind, NodeKind::Observation);
            let producer = producer
                .as_ref()
                .expect("persisted observation must carry a producer envelope");
            assert_eq!(producer.producer_kind, ProducerKind::ObservationWriter);
            assert_eq!(
                producer.producer_components.get("transport"),
                Some(&"mcp".to_owned()),
                "persisted record must be identifiable as MCP-originated"
            );
            let links = evidence_links
                .as_ref()
                .expect("observation must carry evidence links");
            assert!(
                links
                    .iter()
                    .any(|l| l.target_record_id.as_deref() == Some(widget_id)),
                "observation must cite the codegraph symbol"
            );
        }
        other => panic!("record_id must name the Observation node, got {other:?}"),
    }
}

/// fixture over the CLI, record an observation about one of its symbols over
/// MCP, read the observation back through `symbol_context`, and prove the
/// write landed in `agent_memory` with the MCP-origin producer envelope.
#[test]
fn round_trip_write_then_read_back() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let _daemon = start_daemon(&data_dir);
    let widget_id = seed_widget_symbol(&data_dir);
    let client = DaemonClient::from_data_dir(&data_dir).expect("daemon client should connect");
    let server = EgregoreMcpServer::new(data_dir.clone());

    let before_ids = live_record_ids(&client);
    let codegraph_before: Vec<&str> = before_ids
        .iter()
        .filter(|id| id.starts_with("codegraph:"))
        .map(String::as_str)
        .collect();
    assert!(
        !codegraph_before.is_empty(),
        "the seed must produce codegraph records"
    );

    // Write over MCP.
    let raw = server.record_observation(Parameters(valid_args(&data_dir, &widget_id, "codegraph")));
    let payload: Value =
        serde_json::from_str(&raw).expect("record_observation must return a JSON payload");
    assert_eq!(
        payload["ok"],
        Value::from(true),
        "write must succeed: {payload}"
    );
    let record_id = payload["record_id"]
        .as_str()
        .expect("success payload must carry record_id");
    assert!(
        record_id.starts_with("agent_memory:v1:"),
        "record_id must live in the agent_memory domain, got {record_id}"
    );
    // The real tool output validates against the published contract schema.
    assert_valid(
        &response_schema("record_observation").expect("schema must be published"),
        &payload,
        "record_observation success",
    );

    // The store gained agent_memory records only; the deterministic codegraph
    // set is byte-for-byte unchanged (separation by construction).
    let after_ids = live_record_ids(&client);
    let new_ids: Vec<&str> = after_ids
        .iter()
        .filter(|id| !before_ids.contains(id))
        .map(String::as_str)
        .collect();
    assert!(
        !new_ids.is_empty(),
        "the write must add records to the store"
    );
    for id in &new_ids {
        assert!(
            id.starts_with("agent_memory:"),
            "MCP write must not mint records outside agent_memory, got {id}"
        );
    }
    let codegraph_after: Vec<&str> = after_ids
        .iter()
        .filter(|id| id.starts_with("codegraph:"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        codegraph_before, codegraph_after,
        "the deterministic codegraph domain must be unchanged"
    );

    // The persisted observation carries the MCP-origin producer envelope.
    let (records, _, _) = client
        .get_all_records()
        .expect("get_all_records must succeed");
    assert_persisted_mcp_observation(&records, record_id, &widget_id);

    // Retrievable through the existing read path with its citable record_id.
    let ctx_raw = server.symbol_context(Parameters(aletheia_egregore::mcp::SymbolContextArgs {
        symbol_name: "nested::Widget".to_owned(),
        candidate: None,
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        repo_path: None,
    }));
    let ctx: Value =
        serde_json::from_str(&ctx_raw).expect("symbol_context must return a JSON payload");
    assert_eq!(ctx["ok"], Value::from(true), "symbol_context must succeed");
    let observations = ctx["observations"]
        .as_array()
        .expect("symbol_context must carry an observations section");
    assert!(
        observations
            .iter()
            .any(|o| o["record_id"].as_str() == Some(record_id)),
        "the MCP-written observation must be retrievable via symbol_context"
    );
}

/// A well-formed call whose evidence target names nothing live is rejected
/// atomically: the structured error fires and the store is unchanged.
#[test]
fn dangling_evidence_target_rejected_atomically() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let _daemon = start_daemon(&data_dir);
    let _widget_id = seed_widget_symbol(&data_dir);
    let client = DaemonClient::from_data_dir(&data_dir).expect("daemon client should connect");
    let server = EgregoreMcpServer::new(data_dir.clone());

    let before_ids = live_record_ids(&client);
    let raw = server.record_observation(Parameters(valid_args(
        &data_dir,
        DANGLING_SYMBOL_ID,
        "codegraph",
    )));
    let payload: Value =
        serde_json::from_str(&raw).expect("record_observation must return a JSON payload");
    assert_eq!(
        payload["ok"],
        Value::from(false),
        "dangling evidence must be rejected: {payload}"
    );
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("write_rejected"),
        "rejection must carry the frozen write_rejected code: {payload}"
    );
    assert_valid(&error_schema(), &payload, "write_rejected error");
    assert_eq!(
        live_record_ids(&client),
        before_ids,
        "rejected batch must leave the store unchanged"
    );
}

/// Every malformed fixture, replayed against a live daemon, returns the
/// structured `ok:false` envelope and leaves the store byte-for-byte
/// unchanged.
#[test]
fn malformed_calls_against_live_daemon_write_nothing() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let _daemon = start_daemon(&data_dir);
    let widget_id = seed_widget_symbol(&data_dir);
    let client = DaemonClient::from_data_dir(&data_dir).expect("daemon client should connect");
    let server = EgregoreMcpServer::new(data_dir.clone());

    let before_ids = live_record_ids(&client);
    let err_schema = error_schema();
    let validator =
        jsonschema::validator_for(&err_schema).expect("error schema must be a valid JSON Schema");
    for (name, mutate) in malformed_fixtures() {
        let mut args = valid_args(&data_dir, &widget_id, "codegraph");
        mutate(&mut args);
        let raw = server.record_observation(Parameters(args));
        let payload: Value =
            serde_json::from_str(&raw).expect("record_observation must return a JSON payload");
        assert_eq!(
            payload["ok"],
            Value::from(false),
            "{name}: malformed call must be rejected: {payload}"
        );
        let errors: Vec<String> = validator
            .iter_errors(&payload)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{name}: error must validate against the published error schema:\n  - {}\npayload: {payload}",
            errors.join("\n  - ")
        );
    }
    assert_eq!(
        live_record_ids(&client),
        before_ids,
        "no malformed call may write to the store"
    );
}

/// No usable daemon: the tool fails closed with the structured
/// `daemon_not_running` error and writes nothing.
#[test]
fn missing_daemon_returns_structured_daemon_error() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    fs::create_dir_all(&data_dir).expect("should create data dir");
    let server = EgregoreMcpServer::new(data_dir.clone());

    let raw = server.record_observation(Parameters(valid_args(
        &data_dir,
        DANGLING_SYMBOL_ID,
        "codegraph",
    )));
    let payload: Value =
        serde_json::from_str(&raw).expect("record_observation must return a JSON payload");
    assert_eq!(
        payload["ok"],
        Value::from(false),
        "write without a daemon must fail: {payload}"
    );
    assert_eq!(
        payload["error"]["code"].as_str(),
        Some("daemon_not_running"),
        "missing daemon must surface the frozen daemon code: {payload}"
    );
    assert_valid(&error_schema(), &payload, "daemon_not_running error");
}

/// Two identical calls return the same `record_id`: the write is idempotent,
/// keyed by the observation's content-addressed ID.
#[test]
fn identical_retry_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let _daemon = start_daemon(&data_dir);
    let widget_id = seed_widget_symbol(&data_dir);
    let client = DaemonClient::from_data_dir(&data_dir).expect("daemon client should connect");
    let server = EgregoreMcpServer::new(data_dir.clone());

    let first: Value = serde_json::from_str(&server.record_observation(Parameters(valid_args(
        &data_dir,
        &widget_id,
        "codegraph",
    ))))
    .expect("first write must return JSON");
    assert_eq!(first["ok"], Value::from(true), "first write must succeed");
    let after_first = live_record_ids(&client);

    let second: Value = serde_json::from_str(&server.record_observation(Parameters(valid_args(
        &data_dir,
        &widget_id,
        "codegraph",
    ))))
    .expect("retry must return JSON");
    assert_eq!(second["ok"], Value::from(true), "retry must succeed");
    assert_eq!(
        first["record_id"], second["record_id"],
        "identical retry must return the same record_id"
    );
    assert_eq!(
        live_record_ids(&client),
        after_first,
        "identical retry must not duplicate records"
    );
}

// ── CLI parity guard ──────────────────────────────────────────────────────────

/// The CLI's observation builder and the MCP tool validate through the same
/// function: any provenance the CLI accepts, the shared builder accepts, and
/// both stamp the same stable `record_id`.
#[test]
fn cli_provenance_builder_parity() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: "test-agent".to_owned(),
            agent_kind: "other".to_owned(),
            session_id: "sess-001".to_owned(),
            observed_at: "2026-09-26T16:00:00Z".to_owned(),
            source_handle: Some("src/lib.rs".to_owned()),
        },
        text: "Widget::new returns early on zero capacity".to_owned(),
        confidence: 0.85,
        evidence_links: vec![aletheia_egregore::ir::EvidenceLink {
            target_record_id: Some(DANGLING_SYMBOL_ID.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "OBSERVES".to_owned(),
            confidence: "0.85".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }],
        supersession: None,
    };
    // What the CLI builds directly...
    let cli = build_observation_records(&req).expect("CLI builder must succeed");
    // ...is what the MCP tool's pure conversion produces for equivalent args.
    let mcp_req = record_observation_request_from_args(&valid_args(
        Path::new("/tmp/unused"),
        DANGLING_SYMBOL_ID,
        "codegraph",
    ))
    .expect("MCP conversion must succeed");
    let mcp = build_observation_records_with_producer(
        &mcp_req,
        &mcp_observation_producer(&mcp_req.provenance.observed_at),
    )
    .expect("MCP builder must succeed");
    assert_eq!(
        cli.record_id, mcp.record_id,
        "CLI and MCP must agree on the stable record_id for the same observation"
    );
    assert!(
        cli.record_id.starts_with("agent_memory:v1:"),
        "the citable handle lives in the agent_memory domain"
    );
}
