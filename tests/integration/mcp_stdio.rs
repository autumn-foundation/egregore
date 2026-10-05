//! MCP stdio end-to-end smoke test — issue #260.
//!
//! Drives `eg mcp` as a real child process over its rmcp stdio transport
//! against a seeded store with a running daemon, and proves the wire
//! handshake the in-process tests in `mcp.rs` never touch:
//!
//! 1. `initialize` succeeds and identifies the server as `egregore`.
//! 2. `tools/list` returns exactly the five documented tools
//!    (`inspect_store`, `symbol_context`, `task_evidence`, `store_freshness`,
//!    `failure_history`).
//! 3. One `tools/call` returns structured JSON containing a `record_id`
//!    and a repo-relative file/span citation handle.
//!
//! This is the connection smoke check `docs/cli/mcp.md` asks every operator
//! to run after registering the server with their agent host.

#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    adapters::{EmbeddedAletheiaSink, GraphSink},
    daemon::active_metadata,
    ir::{GraphRecord, SourceSpan},
};
use assert_cmd::cargo::cargo_bin;
use serde_json::{Value, json};

/// Maximum time to wait for the daemon to publish active metadata.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time to wait for one JSON-RPC response line from the MCP server.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

const SYMBOL_ID: &str = "codegraph:v4:stdio-smoke-symbol";
const SYMBOL_NAME: &str = "stdio_smoke_probe";
const SYMBOL_PATH: &str = "src/stdio_smoke.rs";

// ── Fixture seeding ───────────────────────────────────────────────────────────

fn seed_store(data_dir: &Path) {
    let mut sink =
        EmbeddedAletheiaSink::open(data_dir).expect("embedded store should open for seeding");
    let symbol = GraphRecord::symbol(
        SYMBOL_ID.to_owned(),
        "function",
        SYMBOL_PATH.to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 64,
            start_line: 7,
            end_line: 14,
            start_column: None,
            end_column: None,
        },
        SYMBOL_NAME.to_owned(),
        format!("Rust function {SYMBOL_NAME}"),
    )
    .with_valid_time_inferred("2026-09-15T00:00:00Z");
    sink.write_record(&symbol)
        .expect("seeded symbol should write");
    sink.persist_indexes()
        .expect("seeded store indexes should persist");
}

// ── Daemon lifecycle ──────────────────────────────────────────────────────────

fn start_daemon(data_dir: &Path) {
    let status = ProcessCommand::new(cargo_bin("egregore"))
        .arg("daemon")
        .arg("start")
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("daemon start should execute");
    assert!(status.success(), "daemon start should succeed");
    let started = Instant::now();
    while started.elapsed() < DAEMON_START_TIMEOUT {
        if active_metadata(data_dir).unwrap_or(None).is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon did not publish active metadata within {DAEMON_START_TIMEOUT:?}");
}

fn stop_daemon(data_dir: &Path) {
    let _ = ProcessCommand::new(cargo_bin("egregore"))
        .arg("daemon")
        .arg("stop")
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if active_metadata(data_dir).unwrap_or(None).is_none() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ── Stdio JSON-RPC session driver ─────────────────────────────────────────────

/// Background line reader so a missing server response fails the test on a
/// timeout instead of blocking the test thread forever.
struct StdioSession {
    stdin: std::process::ChildStdin,
    lines: Receiver<std::io::Result<String>>,
    next_id: u64,
}

impl StdioSession {
    fn spawn(child: &mut Child) -> Self {
        let stdin = child.stdin.take().expect("mcp child stdin should be piped");
        let stdout = child
            .stdout
            .take()
            .expect("mcp child stdout should be piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let Ok(bytes) = reader.read_line(&mut line) else {
                    break;
                };
                let eof = bytes == 0;
                if tx.send(Ok(line)).is_err() || eof {
                    break;
                }
            }
        });
        Self {
            stdin,
            lines: rx,
            next_id: 1,
        }
    }

    fn send(&mut self, payload: &Value) {
        let mut line = serde_json::to_string(payload).expect("request payload should serialize");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("request should write to mcp stdin");
        self.stdin.flush().expect("mcp stdin should flush");
    }

    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(RESPONSE_TIMEOUT)
            .expect("mcp server should answer within timeout")
            .expect("mcp stdout should stay readable");
        serde_json::from_str::<Value>(&line).expect("mcp response should be a JSON object")
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let response = self.recv();
        assert_eq!(
            response["id"], id,
            "response id should match request id, got {response}"
        );
        assert!(
            response.get("error").is_none(),
            "JSON-RPC error for {method}: {}",
            response["error"]
        );
        response
    }
}

fn spawn_mcp_server(data_dir: &Path) -> Child {
    ProcessCommand::new(cargo_bin("egregore"))
        .arg("mcp")
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp server child should spawn")
}

// ── The smoke test ────────────────────────────────────────────────────────────

/// `initialize` → `tools/list` → one `tools/call`, all over the real stdio
/// transport against a seeded store, against the real `eg mcp` binary.
#[test]
fn mcp_stdio_handshake_lists_tools_and_calls_symbol_context() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir: PathBuf = temp.path().join("store");
    seed_store(&data_dir);
    start_daemon(&data_dir);

    let mut child = spawn_mcp_server(&data_dir);
    let mut session = StdioSession::spawn(&mut child);
    let test_result = (|| -> Result<(), String> {
        // 1. initialize must succeed and identify the server.
        let init = session.request(
            "initialize",
            &json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "eg-stdio-smoke-test", "version": "0.0.0" },
            }),
        );
        let server_name = init["result"]["serverInfo"]["name"]
            .as_str()
            .ok_or_else(|| format!("initialize result has no serverInfo.name: {init}"))?;
        if server_name != "egregore" {
            return Err(format!(
                "expected server name 'egregore', got '{server_name}'"
            ));
        }

        // rmcp requires the initialized notification before serving requests.
        session.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

        // 2. tools/list must return exactly the seven documented tools (six
        // reads plus the record_observation write tool, issues #183, #182).
        let list = session.request("tools/list", &json!({}));
        let names: Vec<String> = list["result"]["tools"]
            .as_array()
            .ok_or_else(|| format!("tools/list result has no tools array: {list}"))?
            .iter()
            .map(|t| {
                t["name"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("tools/list entry has no string name: {t}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut sorted = names.clone();
        sorted.sort();
        let expected = vec![
            "failure_history".to_owned(),
            "inspect_store".to_owned(),
            "record_observation".to_owned(),
            "search_code".to_owned(),
            "store_freshness".to_owned(),
            "symbol_at".to_owned(),
            "symbol_context".to_owned(),
            "task_evidence".to_owned(),
        ];
        if sorted != expected {
            return Err(format!(
                "tools/list must return exactly {expected:?}, got {names:?}"
            ));
        }

        // 3. one tools/call must return structured JSON with a record_id and
        //    a repo-relative file/span citation handle.
        let call = session.request(
            "tools/call",
            &json!({
                "name": "symbol_context",
                "arguments": {
                    "symbol_name": SYMBOL_NAME,
                    "data_dir": data_dir.to_string_lossy(),
                },
            }),
        );
        let text = call["result"]["content"][0]["text"]
            .as_str()
            .ok_or_else(|| format!("tools/call result has no text content: {call}"))?;
        let payload: Value = serde_json::from_str(text)
            .map_err(|e| format!("tools/call text content is not JSON: {e}; text={text}"))?;
        if payload["ok"] != true {
            return Err(format!(
                "symbol_context should return ok=true, got {payload}"
            ));
        }
        let facts = payload["source_facts"]
            .as_array()
            .ok_or_else(|| format!("symbol_context returned no source_facts array: {payload}"))?;
        let fact = facts
            .iter()
            .find(|f| f["record_id"].as_str() == Some(SYMBOL_ID))
            .ok_or_else(|| {
                format!("symbol_context source_facts has no record '{SYMBOL_ID}': {payload}")
            })?;
        if fact["repo_relative_path"].as_str() != Some(SYMBOL_PATH) {
            return Err(format!(
                "expected repo_relative_path '{SYMBOL_PATH}', got {}",
                fact["repo_relative_path"]
            ));
        }
        let start_line = fact["span"]["start_line"].as_u64();
        if start_line != Some(7) {
            return Err(format!("expected span.start_line 7, got {start_line:?}"));
        }
        Ok(())
    })();

    // Tear down the child and the daemon even on assertion failure.
    let _ = child.kill();
    let _ = child.wait();
    stop_daemon(&data_dir);

    assert!(test_result.is_ok(), "{}", test_result.unwrap_err());
}
