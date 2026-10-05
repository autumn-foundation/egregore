//! Integration tests for issue #196: `store_coverage` on query responses.
//!
//! Successful `query context` and `query task` responses carry a
//! `store_coverage` map with five boolean domains:
//!
//! ```json
//! "store_coverage": {
//!   "code_graph": true,
//!   "agent_memory": false,
//!   "project": false,
//!   "artifact": false,
//!   "verification": false
//! }
//! ```
//!
//! Meaning:
//! - `false` + empty section: the domain is absent from the store.
//! - `true` + empty section: the domain exists, but the entity has no records.
//!
//! Coverage is structural only; it never implies the code is tested, has no
//! failures, or is safe/correct.
//!
//! Ordering contract (see `docs/cli/query.md`): the CLI lanes (`--graph` and
//! `--data-dir`) serialize the map in declaration order (`code_graph` first);
//! the MCP and daemon lanes build `serde_json::Value` maps, which serialize
//! alphabetically. JSON objects are unordered, so cross-lane comparisons here
//! are semantic (`serde_json::Value` equality); the byte-identity guarantee is
//! per lane (AC6: five consecutive runs over an unchanged store are
//! byte-identical within each lane).
//!
//! Acceptance criteria covered:
//! - AC1: structural (code-only) store reports domain-absent for the four
//!   non-code domains.
//! - AC2: populated store reports entity-absent (all true) for an entity with
//!   no records in those domains.
//! - AC3: `query context` and `query task` CLI responses both carry it, with
//!   exactly the five boolean domains in declaration order.
//! - AC4: error envelopes (`no_match`, `ambiguous_handle`,
//!   `unsupported_handle`) do NOT carry it — probed on the CLI, the MCP
//!   twins, and the daemon verbs with genuine (non-vacuous) triggers.
//! - AC5: MCP twins, embedded (`--data-dir`), and daemon verbs report the
//!   same coverage values as the CLI answers for the same stores.
//! - AC6: 5 consecutive runs yield byte-identical `store_coverage` within
//!   each lane.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

// Re-exported test helpers from the crate.
use aletheia_egregore::ir::GraphRecord;
#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::mcp::{tool_symbol_context_from_records, tool_task_evidence_from_records};

fn eg_bin() -> PathBuf {
    // The test binary is built alongside the `eg` binary.
    let mut p = std::env::current_exe().expect("current exe");
    p.pop(); // deps
    p.pop(); // debug
    p.join("eg")
}

fn run_eg(args: &[&str]) -> Output {
    Command::new(eg_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn eg {args:?}: {e}"))
}

const FIXTURE_REL: &str = "tests/fixtures/query_store_coverage";
const PROBE_SYMBOL: &str = "probe_symbol";
/// GitHub-issue handle bound to `test-task-1` in the full store (via an
/// `EXTERNAL_HANDLE` edge to an `ExternalLink` node; see [`build_stores`]).
const TASK_HANDLE: &str = "https://github.com/acme/repo/issues/1";
/// Handle that matches two tasks at once (see [`build_ambiguous_store`]).
const AMBIGUOUS_HANDLE: &str = "https://github.com/acme/repo/issues/7";
/// Handle with no recognized format: triggers `unsupported_handle`.
const UNSUPPORTED_HANDLE: &str = "not a handle at all";
const UNKNOWN_SYMBOL: &str = "definitely_not_a_symbol_12345";
/// Well-formed task handle with no matching task: triggers `no_match`.
const UNKNOWN_TASK_HANDLE: &str = "https://github.com/acme/repo/issues/99999";

const EXPECTED_STRUCTURAL: &str = r#"{"code_graph":true,"agent_memory":false,"project":false,"artifact":false,"verification":false}"#;
const EXPECTED_FULL: &str =
    r#"{"code_graph":true,"agent_memory":true,"project":true,"artifact":true,"verification":true}"#;

/// Parses an expected coverage JSON string into a `Value` for order-agnostic
/// comparison across lanes (see the module docs on the ordering contract).
fn expected_value(s: &str) -> Value {
    serde_json::from_str(s).expect("expected coverage parses")
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

fn read_fixture(fixture: &Path, name: &str) -> String {
    fs::read_to_string(fixture.join(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// Builds the structural (code-only) and full (all domains) test stores.
///
/// The full store additionally links the `test-task-1` project node to the
/// `TASK_HANDLE` GitHub-issue handle so `query task` has a supported,
/// resolvable handle to exercise. Returns `(structural_path, full_path)`.
fn build_stores(fixture: &Path, work: &Path) -> (PathBuf, PathBuf) {
    let structural = work.join("structural.jsonl");
    let full = work.join("full.jsonl");

    // Structural: code graph records only.
    let code_records = read_fixture(fixture, "code.jsonl");
    fs::write(&structural, &code_records).expect("write structural");

    // Full: code + all other domain records + the task handle link.
    let mut full_content = code_records;
    for domain in &[
        "memory.jsonl",
        "project.jsonl",
        "artifact.jsonl",
        "verification.jsonl",
    ] {
        full_content.push_str(&read_fixture(fixture, domain));
    }
    full_content.push_str(
        r#"{"record_type":"node","id":"task-link-1","kind":"ExternalLink","schema_version":1,"summary":"github issue link","url":"https://github.com/acme/repo/issues/1"}"#,
    );
    full_content.push('\n');
    full_content.push_str(
        r#"{"record_type":"edge","id":"task-edge-1","schema_version":1,"label":"EXTERNAL_HANDLE","source":"test-task-1","target":"task-link-1","summary":"test-task-1 external handle"}"#,
    );
    full_content.push('\n');
    fs::write(&full, &full_content).expect("write full");

    (structural, full)
}

/// Builds a store where one handle matches two tasks, for the genuine
/// `ambiguous_handle` probes.
fn build_ambiguous_store(work: &Path) -> PathBuf {
    let path = work.join("ambiguous.jsonl");
    let content = concat!(
        r#"{"record_type":"node","id":"amb-task-1","kind":"Task","schema_version":1,"summary":"ambiguous task 1","title":"ambiguous task 1"}"#,
        "\n",
        r#"{"record_type":"node","id":"amb-task-2","kind":"Task","schema_version":1,"summary":"ambiguous task 2","title":"ambiguous task 2"}"#,
        "\n",
        r#"{"record_type":"node","id":"amb-link-1","kind":"ExternalLink","schema_version":1,"summary":"shared github link","url":"https://github.com/acme/repo/issues/7"}"#,
        "\n",
        r#"{"record_type":"edge","id":"amb-edge-1","schema_version":1,"label":"EXTERNAL_HANDLE","source":"amb-task-1","target":"amb-link-1","summary":"amb-task-1 external handle"}"#,
        "\n",
        r#"{"record_type":"edge","id":"amb-edge-2","schema_version":1,"label":"EXTERNAL_HANDLE","source":"amb-task-2","target":"amb-link-1","summary":"amb-task-2 external handle"}"#,
        "\n",
    );
    fs::write(&path, content).expect("write ambiguous store");
    path
}

fn load_records(store: &Path) -> Vec<GraphRecord> {
    let jsonl = fs::read_to_string(store).expect("read store");
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("record parses"))
        .collect()
}

/// Extracts `store_coverage` from a CLI JSON response.
fn coverage_from_cli_output(output: &str) -> Value {
    let v: Value = serde_json::from_str(output).expect("CLI output parses as JSON");
    v["store_coverage"].clone()
}

/// Asserts that neither stdout nor stderr carries the coverage signal.
fn assert_no_coverage_text(stdout: &str, stderr: &str, what: &str) {
    assert!(
        !stdout.contains("store_coverage"),
        "{what}: stdout must not carry store_coverage, got: {stdout}"
    );
    assert!(
        !stderr.contains("store_coverage"),
        "{what}: stderr must not carry store_coverage, got: {stderr}"
    );
}

/// AC1: structural store reports domain-absent for non-code domains.
///
/// Queries a symbol that exists in the code graph. The response carries
/// `store_coverage` with `code_graph: true` and all other domains `false`,
/// with byte-identical output across 5 runs (AC6).
#[test]
fn structural_store_reports_domain_absent() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (structural, _full) = build_stores(&fixture, work.path());
    let graph = structural.to_str().expect("utf8").to_owned();

    let mut stdouts = Vec::new();
    for _ in 0..5 {
        let out = run_eg(&["query", "context", PROBE_SYMBOL, "--graph", &graph]);
        assert!(out.status.success(), "query context succeeds");
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        let coverage = coverage_from_cli_output(&stdout);
        assert_eq!(
            coverage,
            expected_value(EXPECTED_STRUCTURAL),
            "structural store: code_graph=true, others=false"
        );
        stdouts.push(stdout);
    }
    // AC6: five repeats are byte-identical.
    for s in &stdouts[1..] {
        assert_eq!(s, &stdouts[0], "5 runs must be byte-identical");
    }
}

/// AC2 + AC3: populated store reports entity-absent; both `query context`
/// and `query task` carry `store_coverage`.
///
/// Queries a symbol/task with no records in a fully-populated store. All
/// five domains report `true` (domains exist, entity has no records).
#[test]
fn populated_store_reports_entity_absent() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (_structural, full) = build_stores(&fixture, work.path());
    let graph = full.to_str().expect("utf8").to_owned();

    // `query context` for the probe symbol (exists in code_graph, but has
    // no observations/project/artifacts/verification records).
    let out = run_eg(&["query", "context", PROBE_SYMBOL, "--graph", &graph]);
    assert!(out.status.success(), "query context succeeds");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let coverage = coverage_from_cli_output(&stdout);
    assert_eq!(
        coverage,
        expected_value(EXPECTED_FULL),
        "populated store: all domains true (entity has no records)"
    );

    // `query task` for the linked task handle: same coverage values.
    let mut stdouts = Vec::new();
    for _ in 0..5 {
        let out = run_eg(&["query", "task", TASK_HANDLE, "--graph", &graph]);
        assert!(out.status.success(), "query task succeeds");
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        let coverage = coverage_from_cli_output(&stdout);
        assert_eq!(
            coverage,
            expected_value(EXPECTED_FULL),
            "populated store task query: all domains true"
        );
        stdouts.push(stdout);
    }
    // AC6: five task-query repeats are byte-identical.
    for s in &stdouts[1..] {
        assert_eq!(s, &stdouts[0], "5 task runs must be byte-identical");
    }
}

/// AC3 (shape): the coverage map has exactly the five boolean domains, in
/// the CLI's fixed declaration order.
#[test]
fn coverage_shape_is_five_boolean_domains() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (structural, _full) = build_stores(&fixture, work.path());
    let graph = structural.to_str().expect("utf8").to_owned();

    let out = run_eg(&["query", "context", PROBE_SYMBOL, "--graph", &graph]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let v: Value = serde_json::from_str(&stdout).expect("parses");

    let coverage = &v["store_coverage"];
    assert!(coverage.is_object(), "store_coverage is an object");
    let obj = coverage.as_object().expect("object");
    assert_eq!(obj.len(), 5, "exactly five domains");
    for domain in &[
        "code_graph",
        "agent_memory",
        "project",
        "artifact",
        "verification",
    ] {
        assert!(
            obj.get(*domain).is_some_and(Value::is_boolean),
            "domain {domain} is a boolean"
        );
    }
    // CLI serializes in declaration order. NOTE: `obj.keys()` cannot prove
    // this — without serde_json/preserve_order a parsed `Map` iterates in
    // BTreeMap (alphabetical) order regardless of document order — so the
    // order is read off the raw stdout bytes instead.
    let order = [
        "code_graph",
        "agent_memory",
        "project",
        "artifact",
        "verification",
    ];
    let mut offsets = Vec::with_capacity(order.len());
    for domain in &order {
        let needle = format!("\"{domain}\"");
        let pos = stdout
            .find(&needle)
            .unwrap_or_else(|| panic!("store_coverage contains {domain}"));
        offsets.push(pos);
    }
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(
        offsets, sorted,
        "CLI store_coverage keys appear in declaration order: {order:?}"
    );
}

/// AC4: CLI error envelopes carry no `store_coverage`.
///
/// Genuine triggers on every probe (no vacuous guards):
/// - `no_match`: unknown symbol (`query context`) and unknown-but-well-formed
///   task handle (`query task`) — exit 2, JSON envelope on stdout.
/// - `unsupported_handle`: handle with no recognized format — exit 1, JSON on
///   stderr.
/// - `ambiguous_handle`: one handle matching two tasks — exit 1, JSON on
///   stderr with both candidates.
#[test]
fn cli_error_envelopes_carry_no_coverage() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (structural, full) = build_stores(&fixture, work.path());
    let structural_graph = structural.to_str().expect("utf8").to_owned();
    let full_graph = full.to_str().expect("utf8").to_owned();
    let ambiguous = build_ambiguous_store(work.path());
    let ambiguous_graph = ambiguous.to_str().expect("utf8").to_owned();

    // no_match: unknown symbol via `query context`.
    let out = run_eg(&[
        "query",
        "context",
        UNKNOWN_SYMBOL,
        "--graph",
        &structural_graph,
    ]);
    assert_eq!(out.status.code(), Some(2), "no_match exits 2");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    let v: Value = serde_json::from_str(&stdout).expect("no_match envelope is JSON");
    assert_eq!(v["ok"], Value::Bool(false));
    assert_eq!(v["error"]["code"], Value::String("no_match".into()));
    assert_no_coverage_text(&stdout, &stderr, "context no_match");

    // no_match: well-formed but unknown task handle via `query task`.
    let out = run_eg(&["query", "task", UNKNOWN_TASK_HANDLE, "--graph", &full_graph]);
    assert_eq!(out.status.code(), Some(2), "task no_match exits 2");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    let v: Value = serde_json::from_str(&stdout).expect("task no_match envelope is JSON");
    assert_eq!(v["ok"], Value::Bool(false));
    assert_eq!(v["error"]["code"], Value::String("no_match".into()));
    assert_no_coverage_text(&stdout, &stderr, "task no_match");

    // unsupported_handle: unrecognized handle format via `query task`.
    let out = run_eg(&["query", "task", UNSUPPORTED_HANDLE, "--graph", &full_graph]);
    assert_eq!(out.status.code(), Some(1), "unsupported_handle exits 1");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    let v: Value =
        serde_json::from_str(&stderr).expect("unsupported_handle envelope is JSON on stderr");
    assert!(
        v.get("Unsupported").is_some(),
        "envelope is the Unsupported variant, got: {v}"
    );
    assert_no_coverage_text(&stdout, &stderr, "task unsupported_handle");

    // ambiguous_handle: one handle matching two tasks via `query task`.
    let out = run_eg(&[
        "query",
        "task",
        AMBIGUOUS_HANDLE,
        "--graph",
        &ambiguous_graph,
    ]);
    assert_eq!(out.status.code(), Some(1), "ambiguous_handle exits 1");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    let v: Value =
        serde_json::from_str(&stderr).expect("ambiguous_handle envelope is JSON on stderr");
    let ambiguous_envelope = v
        .get("Ambiguous")
        .expect("envelope is the Ambiguous variant");
    assert_eq!(
        ambiguous_envelope["candidates"].as_array().map(Vec::len),
        Some(2),
        "both task candidates are reported"
    );
    assert_no_coverage_text(&stdout, &stderr, "task ambiguous_handle");
}

/// AC4 (MCP): the MCP twins return the same error codes as the CLI probes
/// above, and none of their error payloads carries `store_coverage`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn mcp_error_envelopes_carry_no_coverage() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (_structural, full) = build_stores(&fixture, work.path());
    let ambiguous = build_ambiguous_store(work.path());

    let full_records = load_records(&full);
    let ambiguous_records = load_records(&ambiguous);

    let payload = tool_symbol_context_from_records(&full_records, UNKNOWN_SYMBOL);
    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(payload["error"]["code"], Value::String("no_match".into()));
    assert!(
        payload.get("store_coverage").is_none(),
        "MCP symbol no_match must not carry store_coverage"
    );

    let payload = tool_task_evidence_from_records(&full_records, UNKNOWN_TASK_HANDLE);
    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(payload["error"]["code"], Value::String("no_match".into()));
    assert!(
        payload.get("store_coverage").is_none(),
        "MCP task no_match must not carry store_coverage"
    );

    let payload = tool_task_evidence_from_records(&full_records, UNSUPPORTED_HANDLE);
    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(
        payload["error"]["code"],
        Value::String("unsupported_handle".into())
    );
    assert!(
        payload.get("store_coverage").is_none(),
        "MCP unsupported_handle must not carry store_coverage"
    );

    let payload = tool_task_evidence_from_records(&ambiguous_records, AMBIGUOUS_HANDLE);
    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(
        payload["error"]["code"],
        Value::String("ambiguous_handle".into())
    );
    assert_eq!(
        payload["error"]["candidates"].as_array().map(Vec::len),
        Some(2),
        "MCP ambiguous_handle reports both candidates"
    );
    assert!(
        payload.get("store_coverage").is_none(),
        "MCP ambiguous_handle must not carry store_coverage"
    );
}

/// AC5 (MCP): the MCP twins report the same coverage values as the CLI.
///
/// `tool_symbol_context_from_records` and `tool_task_evidence_from_records`
/// are the MCP helper entry points; their `store_coverage` values must match
/// the CLI answers for the same stores (compared semantically — see the
/// module docs on the ordering contract).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn mcp_twins_report_identical_coverage() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (structural, full) = build_stores(&fixture, work.path());

    for (store, expected) in [(&structural, EXPECTED_STRUCTURAL), (&full, EXPECTED_FULL)] {
        let records = load_records(store);

        let symbol_payload = tool_symbol_context_from_records(&records, PROBE_SYMBOL);
        assert_eq!(symbol_payload["ok"], Value::Bool(true));
        assert_eq!(
            symbol_payload["store_coverage"],
            expected_value(expected),
            "MCP symbol_context coverage values must match the CLI answer"
        );

        let task_payload = tool_task_evidence_from_records(&records, TASK_HANDLE);
        // The structural store has no tasks: the twin must return the
        // unchanged no_match envelope with no store_coverage.
        if store == &structural {
            assert_eq!(task_payload["ok"], Value::Bool(false));
            assert_eq!(
                task_payload["error"]["code"],
                Value::String("no_match".into())
            );
            assert!(
                task_payload.get("store_coverage").is_none(),
                "MCP no_match must not carry store_coverage"
            );
        } else {
            assert_eq!(task_payload["ok"], Value::Bool(true));
            assert_eq!(
                task_payload["store_coverage"],
                expected_value(expected),
                "MCP task_evidence coverage values must match the CLI answer"
            );
        }
    }
}

/// AC5 (embedded): `--data-dir` answers report the same coverage values as
/// `--graph` answers for the same store.
///
/// Ingests each store into a real embedded data dir, then queries via the
/// CLI's `--data-dir` lane.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_store_reports_identical_coverage() {
    let fixture = fixture_dir();
    let work = TempDir::new().expect("work tempdir");
    let (structural, full) = build_stores(&fixture, work.path());

    for (store, expected) in [(&structural, EXPECTED_STRUCTURAL), (&full, EXPECTED_FULL)] {
        let data_dir = work.path().join(format!(
            "embedded-{}",
            store.file_stem().expect("stem").to_string_lossy()
        ));
        let data_dir_s = data_dir.to_str().expect("utf8").to_owned();
        // Ingest into the embedded store.
        let out = run_eg(&[
            "ingest",
            store.to_str().expect("utf8"),
            "--adapter",
            "embedded",
            "--data-dir",
            &data_dir_s,
        ]);
        assert!(out.status.success(), "ingest succeeds");

        // Query via --data-dir. Compare parsed Values (order-agnostic);
        // the CLI's declaration order is verified by the --graph tests.
        let out = run_eg(&["query", "context", PROBE_SYMBOL, "--data-dir", &data_dir_s]);
        assert!(out.status.success(), "query context succeeds");
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        let coverage = coverage_from_cli_output(&stdout);
        assert_eq!(
            coverage,
            expected_value(expected),
            "embedded --data-dir coverage values match --graph"
        );

        // 5-run determinism: raw stdout must be byte-identical.
        let mut stdouts = vec![stdout];
        for _ in 1..5 {
            let out = run_eg(&["query", "context", PROBE_SYMBOL, "--data-dir", &data_dir_s]);
            assert!(out.status.success());
            stdouts.push(String::from_utf8(out.stdout).expect("utf8 stdout"));
        }
        for s in &stdouts[1..] {
            assert_eq!(s, &stdouts[0], "embedded 5 runs byte-identical");
        }

        // `query task` through the embedded lane as well.
        let out = run_eg(&["query", "task", TASK_HANDLE, "--data-dir", &data_dir_s]);
        if store == &structural {
            // No tasks in the structural store: no_match, no coverage.
            assert_eq!(out.status.code(), Some(2), "embedded task no_match exits 2");
            let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
            let v: Value = serde_json::from_str(&stdout).expect("envelope is JSON");
            assert_eq!(v["error"]["code"], Value::String("no_match".into()));
            assert!(
                v.get("store_coverage").is_none(),
                "embedded task no_match must not carry store_coverage"
            );
        } else {
            assert!(out.status.success(), "embedded query task succeeds");
            let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
            let coverage = coverage_from_cli_output(&stdout);
            assert_eq!(
                coverage,
                expected_value(expected),
                "embedded task coverage values match --graph"
            );
        }
    }
}

/// AC5 (daemon): direct `DaemonClient` verb coverage.
///
/// Spins up a real daemon per store and drives the public
/// `DaemonClient::query_verb_raw` verbs (`observations_for_symbol` and
/// `criteria_for_task`) the CLI's `--daemon` lane uses. Coverage values must
/// match the CLI answers semantically; five consecutive verb calls must
/// render byte-identical `store_coverage` (AC6).
#[cfg(feature = "embedded-aletheiadb")]
mod daemon_path {
    use std::process::{Child, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use aletheia_egregore::daemon::{DaemonClient, DaemonQueryRejection, runtime_dir_for_data_dir};

    /// A running `egregore daemon run` child, killed on drop.
    struct RunningDaemon {
        child: Child,
    }

    impl Drop for RunningDaemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_daemon(data_dir: &Path) -> (RunningDaemon, DaemonClient) {
        fs::create_dir_all(data_dir).expect("create data dir");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("egregore"));
        command
            .arg("daemon")
            .arg("run")
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--port")
            .arg("0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("daemon should spawn");

        // Wait for the daemon to write its running metadata.
        let metadata_path = runtime_dir_for_data_dir(data_dir).join("egregored.json");
        let start = Instant::now();
        loop {
            if let Ok(contents) = fs::read_to_string(&metadata_path)
                && serde_json::from_str::<Value>(&contents).is_ok()
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "daemon metadata should appear at {}",
                metadata_path.display()
            );
            thread::sleep(Duration::from_millis(50));
        }

        let client =
            DaemonClient::from_data_dir(data_dir).expect("client should connect to the daemon");
        (RunningDaemon { child }, client)
    }

    fn ingest_into_data_dir(store: &Path, data_dir: &Path) {
        let out = run_eg(&[
            "ingest",
            store.to_str().expect("utf8"),
            "--adapter",
            "embedded",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ]);
        assert!(out.status.success(), "ingest succeeds");
    }

    fn rejection_code(err: &anyhow::Error) -> String {
        err.downcast_ref::<DaemonQueryRejection>().map_or_else(
            || format!("unrecognized error: {err:?}"),
            |r| r.code.clone(),
        )
    }

    #[test]
    fn daemon_verbs_report_identical_coverage() {
        let fixture = fixture_dir();
        let work = TempDir::new().expect("work tempdir");
        let (structural, full) = build_stores(&fixture, work.path());

        for (store, expected) in [(&structural, EXPECTED_STRUCTURAL), (&full, EXPECTED_FULL)] {
            let data_dir = work.path().join(format!(
                "daemon-{}",
                store.file_stem().expect("stem").to_string_lossy()
            ));
            ingest_into_data_dir(store, &data_dir);
            let (_daemon, client) = start_daemon(&data_dir);

            // `observations_for_symbol`: coverage values match the CLI.
            let mut rendered = Vec::new();
            for _ in 0..5 {
                let result = client
                    .query_verb_raw(
                        "observations_for_symbol",
                        &serde_json::json!({ "name": PROBE_SYMBOL }),
                        None,
                    )
                    .expect("observations_for_symbol succeeds");
                assert_eq!(
                    result["store_coverage"],
                    expected_value(expected),
                    "daemon symbol coverage values must match the CLI answer"
                );
                rendered.push(
                    serde_json::to_string(&result["store_coverage"]).expect("coverage serializes"),
                );
            }
            // AC6: five verb calls render byte-identical coverage.
            for r in &rendered[1..] {
                assert_eq!(r, &rendered[0], "daemon 5 calls byte-identical");
            }

            // `criteria_for_task`: coverage values match the CLI.
            match client.query_verb_raw(
                "criteria_for_task",
                &serde_json::json!({ "task_id": TASK_HANDLE }),
                None,
            ) {
                Ok(result) => {
                    assert_eq!(
                        result["store_coverage"],
                        expected_value(expected),
                        "daemon task coverage values must match the CLI answer"
                    );
                }
                Err(e) => {
                    // The structural store has no tasks: the verb must reject
                    // with not_found and no coverage payload.
                    assert_eq!(
                        store, &structural,
                        "only the structural store may reject criteria_for_task"
                    );
                    assert_eq!(rejection_code(&e), "not_found");
                }
            }
        }
    }

    #[test]
    fn daemon_error_verbs_carry_no_coverage() {
        let fixture = fixture_dir();
        let work = TempDir::new().expect("work tempdir");
        let (_structural, full) = build_stores(&fixture, work.path());
        let ambiguous = build_ambiguous_store(work.path());

        let data_dir = work.path().join("daemon-errors");
        ingest_into_data_dir(&full, &data_dir);
        let (_daemon, client) = start_daemon(&data_dir);

        // no_match: unknown symbol.
        let err = client
            .query_verb_raw(
                "observations_for_symbol",
                &serde_json::json!({ "name": UNKNOWN_SYMBOL }),
                None,
            )
            .expect_err("unknown symbol must be rejected");
        assert_eq!(rejection_code(&err), "not_found");

        // no_match: unknown task handle.
        let err = client
            .query_verb_raw(
                "criteria_for_task",
                &serde_json::json!({ "task_id": UNKNOWN_TASK_HANDLE }),
                None,
            )
            .expect_err("unknown task handle must be rejected");
        assert_eq!(rejection_code(&err), "not_found");

        // unsupported_handle: unrecognized handle format.
        let err = client
            .query_verb_raw(
                "criteria_for_task",
                &serde_json::json!({ "task_id": UNSUPPORTED_HANDLE }),
                None,
            )
            .expect_err("unsupported handle must be rejected");
        assert_eq!(rejection_code(&err), "bad_request");
        assert!(
            format!("{err:?}").contains("unsupported"),
            "rejection mentions unsupported, got: {err:?}"
        );

        // ambiguous_handle: one handle matching two tasks.
        let amb_dir = work.path().join("daemon-ambiguous");
        ingest_into_data_dir(&ambiguous, &amb_dir);
        let (_amb_daemon, amb_client) = start_daemon(&amb_dir);
        let err = amb_client
            .query_verb_raw(
                "criteria_for_task",
                &serde_json::json!({ "task_id": AMBIGUOUS_HANDLE }),
                None,
            )
            .expect_err("ambiguous handle must be rejected");
        assert_eq!(rejection_code(&err), "bad_request");
        assert!(
            format!("{err:?}").contains("ambiguous"),
            "rejection mentions ambiguous, got: {err:?}"
        );
    }
}
