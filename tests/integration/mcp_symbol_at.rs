//! MCP `symbol_at` temporal lookup tool — issue #181.
//!
//! RED phase: pins the new read-only MCP tool that exposes the temporal
//! symbol-selection axis (`--at` / `--as-of`) to agents. The tool accepts a
//! symbol name, a data directory, and exactly one temporal selector — a Git
//! commit (full SHA or unique prefix) or an RFC 3339 valid-time instant —
//! and returns the citable symbol row for that version: `record_id`, `name`,
//! `kind`, `repo_relative_path`, `span`, `git_commit`, `valid_time`.
//!
//! Coverage contract (mirrors the issue's acceptance criteria):
//! 1. The tool registers as `symbol_at` and the server instructions mention
//!    temporal lookup (so `tools/list` advertises it).
//! 2. Argument validation happens before any daemon contact: exactly one of
//!    `commit` / `as_of` is required (both is a parameter error), `tx_as_of`
//!    is rejected with `not_implemented`, and a malformed `as_of` is
//!    `invalid_timestamp`.
//! 3. Commit selection returns the version's row; a unique prefix resolves;
//!    an ambiguous prefix yields `ambiguous_commit_prefix`; unknown
//!    commits/symbols yield `no_match`.
//! 4. As-of selection returns the version current at the instant; an instant
//!    before history yields `no_match`.
//! 5. The row's seven fields are byte-equal to `eg query symbol --at/--as-of`
//!    JSON for the same fixture history store (the CLI `SymbolResult` row
//!    carries `valid_time` as of issue #181).
//! 6. The row carries exactly the seven citable fields — no raw source text
//!    (redaction-safe) — and output is deterministic across runs.
//! 7. A missing daemon yields the stable `daemon_not_running` envelope (not
//!    a panic), and a live daemon serves the tool end to end.

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
    GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    adapters::{EmbeddedAletheiaSink, GraphSink},
    ir::{EdgeLabel, Graph, IdentitySource, RepositoryIdentityPayload, stable_id},
    mcp::{
        EgregoreMcpServer, SymbolAtArgs, SymbolAtSelector, resolve_symbol_at_selector,
        tool_symbol_at_from_records,
    },
};
use assert_cmd::cargo::cargo_bin;
use rmcp::{ServerHandler as _, handler::server::wrapper::Parameters};
use serde_json::Value;

// ── Fixture: two-commit history for one symbol ──────────────────────────────

const C1: &str = "c1aaaa000000000000000000000000000000000001";
const C2: &str = "c2bbbb000000000000000000000000000000000002";
const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const PATH: &str = "src/lib.rs";
const NAME: &str = "shifty";

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

const fn line_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 64,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn symbol_version(commit: &str, valid_time: &str, span: SourceSpan) -> GraphRecord {
    GraphRecord::node(
        stable_id(&["node", "symbol", "repo_hist", PATH, NAME]),
        NodeKind::Symbol,
        Some(PATH.to_owned()),
        Some(span),
        Some(NAME.to_owned()),
        format!("Symbol {NAME}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

/// Two versions of one symbol identity: lines 1–5 at `C1`, lines 10–20 at `C2`.
fn history_records() -> Vec<GraphRecord> {
    vec![
        symbol_version(C1, T1, line_span(1, 5)),
        symbol_version(C2, T2, line_span(10, 20)),
    ]
}

fn write_history_graph(dir: &Path) -> PathBuf {
    let mut graph = Graph::new();
    for record in history_records() {
        graph.push(record);
    }
    let path = dir.join("history.jsonl");
    fs::write(&path, graph.to_jsonl().expect("graph should serialize")).expect("write graph");
    path
}

/// A same-name/same-commit collision across two repositories, built from the
/// containment topology `RepositoryIndex` attributes (mirrors the
/// `query::repo` unit-test fixture shape).
fn two_repo_records() -> Vec<GraphRecord> {
    let repo_a = "codegraph:v4:repo-alpha";
    let repo_b = "codegraph:v4:repo-beta";
    let file_a = "codegraph:v4:file-alpha";
    let file_b = "codegraph:v4:file-beta";
    let sym_a = "codegraph:v4:sym-alpha";
    let sym_b = "codegraph:v4:sym-beta";
    let repo_node = |id: &str, basename: &str| {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(basename.to_owned()),
            format!("Repository {basename}"),
        )
        .with_repository_identity(RepositoryIdentityPayload {
            identity_source: IdentitySource::OperatorOverride,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: None,
            basename: basename.to_owned(),
        })
    };
    let code_edge = |label: EdgeLabel, source: &str, target: &str| {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            None,
            format!("{label:?} {source} -> {target}"),
        )
    };
    let file_node = |id: &str| {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(PATH.to_owned()),
            None,
            Some(PATH.to_owned()),
            format!("File {PATH}"),
        )
    };
    let sym_node = |id: &str| {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(PATH.to_owned()),
            Some(line_span(1, 5)),
            Some(NAME.to_owned()),
            format!("Symbol {NAME}"),
        )
        .with_temporal(temporal(C1, T1))
    };
    vec![
        repo_node(repo_a, "alpha"),
        repo_node(repo_b, "beta"),
        file_node(file_a),
        file_node(file_b),
        sym_node(sym_a),
        sym_node(sym_b),
        code_edge(EdgeLabel::Contains, repo_a, file_a),
        code_edge(EdgeLabel::Contains, repo_b, file_b),
        code_edge(EdgeLabel::Defines, file_a, sym_a),
        code_edge(EdgeLabel::Defines, file_b, sym_b),
    ]
}

// ── Small assertion helpers ─────────────────────────────────────────────────

fn error_code(payload: &Value) -> &str {
    payload["error"]["code"].as_str().unwrap_or("<missing>")
}

fn symbol_at_args(symbol_name: &str, commit: Option<&str>, as_of: Option<&str>) -> SymbolAtArgs {
    SymbolAtArgs {
        symbol_name: symbol_name.to_owned(),
        commit: commit.map(str::to_owned),
        as_of: as_of.map(str::to_owned),
        tx_as_of: None,
        data_dir: Some("/nonexistent-egregore-data-dir-issue-181".to_owned()),
        repo_path: None,
    }
}

fn call_symbol_at(args: SymbolAtArgs) -> Value {
    let server = EgregoreMcpServer::new(Path::new(".").to_path_buf());
    let raw = server.symbol_at(Parameters(args));
    serde_json::from_str(&raw).expect("symbol_at must return a JSON payload")
}

// ── §1 Registration ─────────────────────────────────────────────────────────

#[test]
fn tool_registers_as_symbol_at_with_temporal_description() {
    let attr = EgregoreMcpServer::symbol_at_tool_attr();
    assert_eq!(
        attr.name.as_ref(),
        "symbol_at",
        "tool must register as `symbol_at`"
    );
    let description = attr.description.as_deref().unwrap_or_default();
    assert!(
        description.contains("temporal"),
        "tools/list description must name the temporal lookup, got: {description}"
    );
}

#[test]
fn server_instructions_mention_temporal_lookup() {
    let server = EgregoreMcpServer::new(Path::new(".").to_path_buf());
    let instructions = server.get_info().instructions.unwrap_or_default();
    assert!(
        instructions.contains("symbol_at") && instructions.contains("temporal"),
        "server instructions must advertise the temporal lookup tool, got: {instructions}"
    );
}

// ── §2 Selector validation (pure, no daemon) ─────────────────────────────────

#[test]
fn commit_selector_resolves() {
    assert!(
        matches!(
            resolve_symbol_at_selector(Some("c1a"), None, None),
            Ok(SymbolAtSelector::Commit("c1a"))
        ),
        "a lone commit selector must resolve"
    );
}

#[test]
fn as_of_selector_resolves() {
    assert!(
        matches!(
            resolve_symbol_at_selector(None, Some(T1), None),
            Ok(SymbolAtSelector::AsOf(T1))
        ),
        "a lone as_of selector must resolve"
    );
}

#[test]
fn both_selectors_are_rejected_as_a_parameter_error() {
    let err = resolve_symbol_at_selector(Some(C1), Some(T1), None)
        .expect_err("commit + as_of must be rejected");
    assert_eq!(err["ok"], Value::from(false));
    assert_eq!(err["error"]["code"], Value::from("bad_request"));
}

#[test]
fn neither_selector_is_rejected_as_a_parameter_error() {
    let err =
        resolve_symbol_at_selector(None, None, None).expect_err("no selector must be rejected");
    assert_eq!(err["error"]["code"], Value::from("bad_request"));
}

#[test]
fn blank_selectors_are_treated_as_absent() {
    assert!(
        matches!(
            resolve_symbol_at_selector(Some("  "), Some(T1), None),
            Ok(SymbolAtSelector::AsOf(T1))
        ),
        "a whitespace-only commit must not shadow a real as_of selector"
    );
}

#[test]
fn tx_as_of_is_rejected_as_not_implemented() {
    let err =
        resolve_symbol_at_selector(None, None, Some(T1)).expect_err("tx_as_of must be rejected");
    assert_eq!(err["error"]["code"], Value::from("not_implemented"));
    // `tx_as_of` wins over the both/neither check: the axis itself is absent.
    let err_both = resolve_symbol_at_selector(Some(C1), None, Some(T1))
        .expect_err("tx_as_of + commit must still be not_implemented");
    assert_eq!(err_both["error"]["code"], Value::from("not_implemented"));
}

#[test]
fn malformed_as_of_is_rejected_as_invalid_timestamp() {
    let err = resolve_symbol_at_selector(None, Some("yesterday"), None)
        .expect_err("malformed as_of must be rejected");
    assert_eq!(err["error"]["code"], Value::from("invalid_timestamp"));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid --as-of timestamp"),
        "message must mirror the CLI diagnostic, got: {}",
        err["error"]["message"]
    );
}

// ── §3 Method validation happens before daemon contact ──────────────────────

#[test]
fn method_empty_symbol_name_is_missing_argument() {
    let payload = call_symbol_at(symbol_at_args("", Some(C1), None));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "missing_argument");
}

#[test]
fn method_tx_as_of_is_not_implemented_without_touching_the_daemon() {
    let mut args = symbol_at_args(NAME, None, None);
    args.tx_as_of = Some(T1.to_owned());
    let payload = call_symbol_at(args);
    assert_eq!(error_code(&payload), "not_implemented");
}

#[test]
fn method_both_selectors_is_a_parameter_error() {
    let payload = call_symbol_at(symbol_at_args(NAME, Some(C1), Some(T1)));
    assert_eq!(error_code(&payload), "bad_request");
}

#[test]
fn method_neither_selector_is_a_parameter_error() {
    let payload = call_symbol_at(symbol_at_args(NAME, None, None));
    assert_eq!(error_code(&payload), "bad_request");
}

#[test]
fn method_malformed_as_of_is_invalid_timestamp_without_touching_the_daemon() {
    let payload = call_symbol_at(symbol_at_args(NAME, None, Some("not-a-time")));
    assert_eq!(error_code(&payload), "invalid_timestamp");
}

#[test]
fn method_missing_daemon_yields_the_stable_daemon_envelope() {
    let payload = call_symbol_at(symbol_at_args(NAME, Some(C1), None));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(
        error_code(&payload),
        "daemon_not_running",
        "a missing daemon must surface the stable staleness envelope, got: {payload}"
    );
}

// ── §4 Pure query: commit selection ─────────────────────────────────────────

fn commit_payload(symbol_name: &str, commit: &str) -> Value {
    tool_symbol_at_from_records(
        &history_records(),
        symbol_name,
        SymbolAtSelector::Commit(commit),
    )
}

#[test]
fn at_commit_returns_the_version_row() {
    let payload = commit_payload(NAME, C1);
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    let symbol = &payload["symbol"];
    assert_eq!(
        symbol["record_id"],
        stable_id(&["node", "symbol", "repo_hist", PATH, NAME]).as_str()
    );
    assert_eq!(symbol["name"], NAME);
    assert_eq!(symbol["kind"], "Symbol");
    assert_eq!(symbol["repo_relative_path"], PATH);
    assert_eq!(symbol["span"]["start_line"], 1);
    assert_eq!(symbol["span"]["end_line"], 5);
    assert_eq!(symbol["git_commit"], C1);
    assert_eq!(symbol["valid_time"], T1);
    assert_eq!(payload["symbol_name"], NAME);
    assert_eq!(payload["selector"]["kind"], "commit");
    assert_eq!(payload["selector"]["value"], C1);
}

#[test]
fn at_commit_unique_prefix_resolves() {
    let payload = commit_payload(NAME, &C1[..12]);
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C1);
    assert_eq!(payload["symbol"]["span"]["start_line"], 1);
}

#[test]
fn at_commit_second_version_returns_newer_span() {
    let payload = commit_payload(NAME, C2);
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C2);
    assert_eq!(payload["symbol"]["span"]["start_line"], 10);
    assert_eq!(payload["symbol"]["valid_time"], T2);
}

#[test]
fn ambiguous_commit_prefix_yields_ambiguous_commit_prefix() {
    // "c" prefixes both C1 and C2 — deterministic, no git luck involved.
    let payload = commit_payload(NAME, "c");
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "ambiguous_commit_prefix");
    assert_eq!(payload["error"]["commit_prefix"], "c");
    assert_eq!(payload["error"]["matching_commits"], 2);
}

#[test]
fn unknown_commit_prefix_is_no_match() {
    let payload = commit_payload(NAME, "ff");
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "no_match");
}

#[test]
fn unknown_symbol_is_no_match() {
    let payload = commit_payload("no_such_symbol", C1);
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "no_match");
    assert_eq!(payload["error"]["symbol_name"], "no_such_symbol");
}

// ── §4 Pure query: as-of selection ──────────────────────────────────────────

fn as_of_payload(symbol_name: &str, as_of: &str) -> Value {
    tool_symbol_at_from_records(
        &history_records(),
        symbol_name,
        SymbolAtSelector::AsOf(as_of),
    )
}

#[test]
fn as_of_between_commits_returns_the_older_version() {
    let payload = as_of_payload(NAME, "2026-01-01T12:00:00Z");
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C1);
    assert_eq!(payload["symbol"]["span"]["start_line"], 1);
    assert_eq!(payload["symbol"]["valid_time"], T1);
    assert_eq!(payload["selector"]["kind"], "as_of");
    assert_eq!(payload["selector"]["value"], "2026-01-01T12:00:00Z");
}

#[test]
fn as_of_at_second_commit_returns_the_newer_version() {
    let payload = as_of_payload(NAME, T2);
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C2);
    assert_eq!(payload["symbol"]["span"]["start_line"], 10);
}

#[test]
fn as_of_before_history_is_no_match() {
    let payload = as_of_payload(NAME, "2025-01-01T00:00:00Z");
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "no_match");
}

#[test]
fn multi_repo_collision_yields_ambiguous_repository() {
    let payload =
        tool_symbol_at_from_records(&two_repo_records(), NAME, SymbolAtSelector::Commit(C1));
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(error_code(&payload), "ambiguous_repository");
    let repositories = payload["error"]["repositories"]
        .as_array()
        .expect("ambiguous_repository must list repositories");
    assert_eq!(repositories.len(), 2, "both repositories must be listed");
}

// ── §4 Pure query: shape, determinism, redaction ────────────────────────────

#[test]
fn row_carries_exactly_the_seven_citable_fields() {
    let payload = commit_payload(NAME, C1);
    let symbol = payload["symbol"]
        .as_object()
        .expect("symbol must be an object");
    let mut keys: Vec<&str> = symbol.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "git_commit",
            "kind",
            "name",
            "record_id",
            "repo_relative_path",
            "span",
            "valid_time"
        ],
        "the row must carry exactly the citable fields — no raw source text"
    );
}

#[test]
fn payload_carries_no_raw_source_text() {
    let payload = commit_payload(NAME, C1);
    let serialized = serde_json::to_string(&payload).expect("payload must serialize");
    for forbidden in [
        "fn shifty",
        "signature",
        "\"doc\"",
        "\"text\"",
        "\"summary\"",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "payload must not leak source text or doc fields; found {forbidden:?}"
        );
    }
}

#[test]
fn output_is_deterministic_across_runs() {
    let first = serde_json::to_string(&commit_payload(NAME, C1)).expect("serialize");
    let second = serde_json::to_string(&commit_payload(NAME, C1)).expect("serialize");
    assert_eq!(
        first, second,
        "two runs over the same records must be byte-identical"
    );
    let as_of_first = serde_json::to_string(&as_of_payload(NAME, T2)).expect("serialize");
    let as_of_second = serde_json::to_string(&as_of_payload(NAME, T2)).expect("serialize");
    assert_eq!(as_of_first, as_of_second);
}

// ── §5 CLI parity: byte-equal fields for the same store and inputs ──────────

fn cli_symbol_json(name: &str, selector: &str, value: &str, graph: &Path) -> Value {
    let output = ProcessCommand::new(cargo_bin("egregore"))
        .args(["query", "symbol", name, selector, value, "--graph"])
        .arg(graph)
        .output()
        .expect("eg query symbol should execute");
    assert!(
        output.status.success(),
        "CLI should succeed for {selector}={value}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    serde_json::from_str(stdout.trim()).expect("CLI should print one JSON row")
}

fn assert_row_matches_cli(tool_row: &Value, cli_row: &Value) {
    for field in [
        "record_id",
        "name",
        "kind",
        "repo_relative_path",
        "span",
        "git_commit",
        "valid_time",
    ] {
        assert_eq!(
            tool_row[field], cli_row[field],
            "field `{field}` must be byte-equal between MCP and CLI"
        );
    }
}

#[test]
fn at_commit_row_is_byte_equal_to_cli() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_history_graph(temp.path());
    let cli_row = cli_symbol_json(NAME, "--at", C1, &graph);
    let payload = commit_payload(NAME, C1);
    assert_eq!(payload["ok"], Value::from(true));
    assert_row_matches_cli(&payload["symbol"], &cli_row);
}

#[test]
fn as_of_row_is_byte_equal_to_cli() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_history_graph(temp.path());
    let cli_row = cli_symbol_json(NAME, "--as-of", "2026-01-01T12:00:00Z", &graph);
    let payload = as_of_payload(NAME, "2026-01-01T12:00:00Z");
    assert_eq!(payload["ok"], Value::from(true));
    assert_row_matches_cli(&payload["symbol"], &cli_row);
}

#[test]
fn ambiguous_prefix_matches_cli_exit_code() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_history_graph(temp.path());
    let output = ProcessCommand::new(cargo_bin("egregore"))
        .args(["query", "symbol", NAME, "--at", "c", "--graph"])
        .arg(&graph)
        .output()
        .expect("eg query symbol should execute");
    assert_eq!(
        output.status.code(),
        Some(1),
        "CLI reports an ambiguous prefix with exit 1: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = commit_payload(NAME, "c");
    assert_eq!(error_code(&payload), "ambiguous_commit_prefix");
}

// ── §6 Contract conformance for the new tool ────────────────────────────────

#[test]
fn symbol_at_success_conforms_to_published_schema() {
    use aletheia_egregore::mcp::stamp_freshness_on_payload;
    use aletheia_egregore::mcp_contract::response_schema;
    let mut payload = commit_payload(NAME, C1);
    assert_eq!(payload["ok"], Value::from(true));
    stamp_freshness_on_payload(&mut payload, &history_records(), Path::new("."), None);
    let schema = response_schema("symbol_at").expect("symbol_at must have a schema");
    let validator = jsonschema::validator_for(&schema).expect("schema must be valid");
    let errors: Vec<String> = validator
        .iter_errors(&payload)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "symbol_at success failed schema validation:\n  - {}\n{payload}",
        errors.join("\n  - ")
    );
}

#[test]
fn symbol_at_errors_conform_to_published_error_schema() {
    use aletheia_egregore::mcp::missing_argument_error;
    use aletheia_egregore::mcp_contract::error_schema;
    let schema = error_schema();
    let validator = jsonschema::validator_for(&schema).expect("schema must be valid");
    let check = |payload: &Value, what: &str| {
        let errors: Vec<String> = validator
            .iter_errors(payload)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{what} failed error-schema validation:\n  - {}\n{payload}",
            errors.join("\n  - ")
        );
    };
    check(&commit_payload(NAME, "c"), "ambiguous_commit_prefix");
    check(&commit_payload(NAME, "ff"), "no_match");
    check(&commit_payload("no_such_symbol", C1), "no_match");
    check(
        &tool_symbol_at_from_records(&two_repo_records(), NAME, SymbolAtSelector::Commit(C1)),
        "ambiguous_repository",
    );
    check(
        &resolve_symbol_at_selector(Some(C1), Some(T1), None).expect_err("both"),
        "bad_request",
    );
    check(
        &resolve_symbol_at_selector(None, None, Some(T1)).expect_err("tx_as_of"),
        "not_implemented",
    );
    check(
        &resolve_symbol_at_selector(None, Some("bogus"), None).expect_err("bad as_of"),
        "invalid_timestamp",
    );
    check(&missing_argument_error("symbol_name"), "missing_argument");
}

// ── §7 Daemon: staleness envelope and end-to-end ─────────────────────────────

fn seed_store(data_dir: &Path) {
    let mut sink =
        EmbeddedAletheiaSink::open(data_dir).expect("embedded store should open for seeding");
    for record in history_records() {
        sink.write_record(&record)
            .expect("seeded history record should write");
    }
    sink.persist_indexes()
        .expect("seeded store indexes should persist");
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

fn read_daemon_state(data_dir: &Path) -> Option<String> {
    let metadata_path = runtime_dir(data_dir).join("egregored.json");
    let contents = fs::read_to_string(metadata_path).ok()?;
    serde_json::from_str::<Value>(&contents)
        .ok()?
        .get("state")?
        .as_str()
        .map(ToOwned::to_owned)
}

struct RunningDaemon {
    child: Child,
    data_dir: PathBuf,
}

impl RunningDaemon {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = ProcessCommand::new(cargo_bin("egregore"))
            .args(["daemon", "stop", "--data-dir"])
            .arg(&self.data_dir)
            .output();
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn start_daemon(data_dir: &Path) -> RunningDaemon {
    fs::create_dir_all(data_dir).expect("should create data dir");
    let mut command = ProcessCommand::new(cargo_bin("egregore"));
    command
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("daemon should spawn");
    let start = Instant::now();
    loop {
        if read_daemon_state(data_dir).as_deref() == Some("running") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "daemon should reach running state for {}",
            data_dir.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
    RunningDaemon {
        child,
        data_dir: data_dir.to_path_buf(),
    }
}

#[test]
fn daemon_serves_symbol_at_end_to_end() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("egregore-data");
    seed_store(&data_dir);
    let _daemon = start_daemon(&data_dir);

    let server = EgregoreMcpServer::new(Path::new(".").to_path_buf());
    let raw = server.symbol_at(Parameters(SymbolAtArgs {
        symbol_name: NAME.to_owned(),
        commit: Some(C1.to_owned()),
        as_of: None,
        tx_as_of: None,
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        repo_path: None,
    }));
    let payload: Value = serde_json::from_str(&raw).expect("symbol_at must return JSON");
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C1);
    assert_eq!(payload["symbol"]["valid_time"], T1);
    assert_eq!(payload["symbol"]["span"]["start_line"], 1);
    assert!(
        payload.get("freshness").is_some(),
        "successful reads carry the freshness stamp: {payload}"
    );

    // The as-of axis works through the daemon too.
    let raw = server.symbol_at(Parameters(SymbolAtArgs {
        symbol_name: NAME.to_owned(),
        commit: None,
        as_of: Some("2026-01-01T12:00:00Z".to_owned()),
        tx_as_of: None,
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        repo_path: None,
    }));
    let payload: Value = serde_json::from_str(&raw).expect("symbol_at must return JSON");
    assert_eq!(payload["ok"], Value::from(true), "got: {payload}");
    assert_eq!(payload["symbol"]["git_commit"], C1);
}

/// The example schema generator covers the new tool, so the checked-in
/// `docs/schema/mcp/symbol_at.schema.json` cannot drift unnoticed.
#[test]
fn published_schema_file_covers_symbol_at() {
    use aletheia_egregore::mcp_contract::response_schema;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/schema/mcp");
    let path = dir.join("symbol_at.schema.json");
    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("published schema file missing: {}: {e}", path.display()));
    let from_file: Value = serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()));
    assert_eq!(
        from_file,
        response_schema("symbol_at").expect("symbol_at must have a registered schema"),
        "checked-in symbol_at.schema.json drifted from mcp_contract::response_schema"
    );
}
