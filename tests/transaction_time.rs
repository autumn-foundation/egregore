#![allow(missing_docs)]

//! Transaction-time query workflow tests (issue #66).
//!
//! These exercise `eg query symbol --tx-as-of <instant>` over a seeded local
//! store that mixes a code fact, an agent-memory observation, a project task, a
//! verification record, and a *later correction* to the same logical symbol.
//!
//! Covered acceptance criteria:
//! - AC1/AC2: a prior transaction-time view excludes later corrections.
//! - AC4: valid-time and transaction-time axes apply independently.
//! - AC5: every returned row carries the required handles.
//! - AC6: malformed/out-of-range/unsupported queries produce stable diagnostics
//!   and never silently fall back to current state.
//! - AC7: repeating the query is deterministic.
//! - AC8: output never includes raw record bodies.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    GraphRecord, SourceSpan,
    ir::{Graph, NodeKind, stable_id},
};
use assert_cmd::Command as CargoCommand;
use serde_json::Value;

const SECRET_MARKER: &str = "TOPSECRET_RAW_BODY_PAYLOAD";

const V1_TX: &str = "2026-01-01T00:00:00Z";
const V2_TX: &str = "2026-01-03T00:00:00Z";
const V1_VT: &str = "2026-01-01T00:00:00Z";
const V2_VT: &str = "2026-01-03T00:00:00Z";

/// Seeds a multi-domain store: a code fact (symbol `widget`) plus a later
/// correction to the same symbol, an agent observation, a project task, and a
/// verification record. Returns the JSONL path.
fn seed_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("store.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "widget"]);

    // Code fact v1 — committed at V1_TX, true at V1_VT.
    let v1 = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        format!("widget v1 {SECRET_MARKER}"),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX);

    // Correction v2 (same logical id) — committed later at V2_TX.
    let v2 = GraphRecord::symbol(
        sym_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 7),
        "widget".to_owned(),
        format!("widget v2 corrected {SECRET_MARKER}"),
    )
    .with_node_time(V2_VT, "author_provided", V2_TX)
    .with_transaction_time(V2_TX);

    // Other domains (present so the store is realistic; inert for symbol query).
    // Each carries its domain's own schema version so the store validates.
    let observation = GraphRecord::node(
        "agent_memory:v1:obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("observation about widget {SECRET_MARKER}"),
    )
    .with_domain("agent_memory", 1)
    .with_transaction_time(V1_TX);
    let task = GraphRecord::node(
        "project:v1:task-1".to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("ship widget".to_owned()),
        format!("task body {SECRET_MARKER}"),
    )
    .with_domain("project", 1)
    .with_transaction_time(V1_TX);
    let verification = GraphRecord::node(
        "verification:v1:ver-1".to_owned(),
        NodeKind::Verification,
        None,
        None,
        None,
        format!("verification log {SECRET_MARKER}"),
    )
    .with_domain("verification", 1)
    .with_transaction_time(V2_TX);

    let mut graph = Graph::new();
    graph.push(v1);
    graph.push(v2);
    graph.push(observation);
    graph.push(task);
    graph.push(verification);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn run_tx_query(graph: &PathBuf, extra: &[&str]) -> (bool, Value) {
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "widget", "--graph"])
        .arg(graph)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    let success = output.status.success();
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let value: Value = serde_json::from_str(stdout.trim()).expect("valid JSON envelope");
    (success, value)
}

// ── AC1 / AC2: prior view excludes later corrections ────────────────────────

#[test]
fn tx_as_of_returns_prior_view_excluding_later_correction() {
    let (_t, graph) = seed_store();
    let (ok, env) = run_tx_query(&graph, &["--tx-as-of", "2026-01-02T00:00:00Z"]);
    assert!(ok, "query should succeed");
    assert_eq!(env["ok"], true);
    let records = env["records"].as_array().expect("records");
    assert_eq!(records.len(), 1, "exactly one symbol version known by T");
    assert_eq!(
        records[0]["transaction_time"], V1_TX,
        "must be the v1 version"
    );
    assert_eq!(records[0]["span"]["end_line"], 5, "v1 span, not v2");
}

#[test]
fn tx_as_of_after_correction_returns_latest_known_version() {
    let (_t, graph) = seed_store();
    let (ok, env) = run_tx_query(&graph, &["--tx-as-of", "2026-01-04T00:00:00Z"]);
    assert!(ok);
    let records = env["records"].as_array().expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["transaction_time"], V2_TX,
        "latest known version"
    );
    assert_eq!(records[0]["span"]["end_line"], 7, "v2 span");
}

// ── AC5: every row carries the required handles ─────────────────────────────

#[test]
fn tx_as_of_row_carries_required_handles() {
    let (_t, graph) = seed_store();
    let (_ok, env) = run_tx_query(&graph, &["--tx-as-of", "2026-01-02T00:00:00Z"]);
    let row = &env["records"][0];
    for field in [
        "record_id",
        "schema_version",
        "name",
        "kind",
        "domain",
        "trust_class",
        "repo_relative_path",
        "span",
        "valid_time",
        "transaction_time",
    ] {
        assert!(!row[field].is_null(), "row must carry `{field}`: {row}");
    }
    assert_eq!(row["kind"], "Symbol");
    assert_eq!(row["trust_class"], "source_fact");
    assert_eq!(row["domain"], "codegraph");
    // The view-level transaction handle is the requested instant.
    assert_eq!(env["snapshot"], "2026-01-02T00:00:00Z");
}

// ── AC4: valid-time and transaction-time axes are independent ───────────────

#[test]
fn tx_and_valid_axes_apply_independently() {
    let (_t, graph) = seed_store();
    // Known by 2026-01-04 (so v2 IS in the store), but asking what was TRUE at
    // valid-time 2026-01-02 → v2 (valid 2026-01-03) is not yet true, so v1 wins.
    let (ok, env) = run_tx_query(
        &graph,
        &[
            "--tx-as-of",
            "2026-01-04T00:00:00Z",
            "--as-of",
            "2026-01-02T00:00:00Z",
        ],
    );
    assert!(ok);
    let records = env["records"].as_array().expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["valid_time"], V1_VT,
        "valid-time axis pins the answer to v1 even though v2 is known"
    );
    assert_eq!(env["as_of"], "2026-01-02T00:00:00Z");
}

// ── AC6: diagnostics, never a silent current-state fallback ─────────────────

#[test]
fn tx_as_of_before_first_transaction_is_empty_with_diagnostic() {
    let (_t, graph) = seed_store();
    let (ok, env) = run_tx_query(&graph, &["--tx-as-of", "2025-01-01T00:00:00Z"]);
    assert!(
        ok,
        "out-of-range is a structured result, not a process error"
    );
    assert_eq!(env["records"].as_array().map(Vec::len), Some(0));
    assert!(
        diagnostic_codes(&env).contains(&"before_first_transaction".to_owned()),
        "must report before_first_transaction, got {env}"
    );
}

#[test]
fn tx_as_of_after_latest_transaction_annotates_full_view() {
    let (_t, graph) = seed_store();
    let (ok, env) = run_tx_query(&graph, &["--tx-as-of", "2026-06-01T00:00:00Z"]);
    assert!(ok);
    assert_eq!(env["records"].as_array().map(Vec::len), Some(1));
    assert!(
        diagnostic_codes(&env).contains(&"after_latest_transaction".to_owned()),
        "must annotate after_latest_transaction, got {env}"
    );
}

#[test]
fn tx_as_of_malformed_timestamp_returns_error_envelope() {
    let (_t, graph) = seed_store();
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&graph)
        .args(["--tx-as-of", "not-a-timestamp"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let env: Value = serde_json::from_str(stdout.trim()).expect("JSON");
    assert_eq!(env["ok"], false);
    assert_eq!(env["error"]["code"], "invalid_timestamp");
}

#[test]
fn tx_as_of_with_at_is_unsupported_combination() {
    let (_t, graph) = seed_store();
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&graph)
        .args(["--tx-as-of", "2026-01-02T00:00:00Z", "--at", "abc1234"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let env: Value = serde_json::from_str(stdout.trim()).expect("JSON");
    assert_eq!(env["ok"], false);
    assert_eq!(env["error"]["code"], "unsupported_combination");
}

// ── AC7: determinism across repeated runs ───────────────────────────────────

#[test]
fn tx_as_of_is_deterministic_across_five_runs() {
    let (_t, graph) = seed_store();
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let output = CargoCommand::cargo_bin("egregore")
            .expect("egregore binary")
            .args(["query", "symbol", "widget", "--graph"])
            .arg(&graph)
            .args(["--tx-as-of", "2026-01-04T00:00:00Z"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(String::from_utf8(output).expect("utf8"));
    }
    assert!(
        outputs.windows(2).all(|w| w[0] == w[1]),
        "repeated transaction-time queries must be byte-identical"
    );
}

// ── AC8: output never includes raw record bodies ────────────────────────────

#[test]
fn tx_as_of_output_excludes_raw_bodies() {
    let (_t, graph) = seed_store();
    let output = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&graph)
        .args(["--tx-as-of", "2026-06-01T00:00:00Z"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        !stdout.contains(SECRET_MARKER),
        "transaction-time output must not leak raw record bodies"
    );
}

// ── AC2: a later tombstone does not erase the prior transaction-time view ────
// Exercised against the library directly so the test can use a current-state
// tombstone (which carries no transaction-time stamp of its own).

#[test]
fn tx_as_of_ignores_later_tombstone() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "widget"]);
    let v1 = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        "widget v1".to_owned(),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX);
    // A later current-state deletion of the same symbol.
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:codegraph:v4:widget".to_owned(),
        schema_version: 4,
        deleted_id: sym_id,
        summary: "widget removed".to_owned(),
        producer: None,
    };
    let records = vec![v1, tombstone];

    // Querying before the deletion still returns the symbol the store knew then.
    let result = symbol_as_of_transaction_time(&records, "widget", "2026-01-02T00:00:00Z", None)
        .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "later tombstone must not erase the prior transaction-time view"
    );
    let expected_id = stable_id(&["node", "Symbol", "src/lib.rs", "widget"]);
    assert_eq!(result.records[0].id(), expected_id.as_str());
}

// ── AC6: a matched record lacking transaction metadata is excluded, not
//        silently treated as current state (library-level check).

#[test]
fn tx_as_of_excludes_record_without_transaction_metadata() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // A symbol carrying a git-commit valid time but NO transaction-time stamp.
    let sym = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "widget"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        "widget".to_owned(),
    );
    let records = vec![sym];

    let result = symbol_as_of_transaction_time(&records, "widget", "2026-01-02T00:00:00Z", None)
        .expect("query ok");
    assert!(
        result.records.is_empty(),
        "record without transaction metadata must be excluded (no current-state fallback)"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_transaction_metadata"),
        "missing metadata must be reported as a diagnostic"
    );
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn diagnostic_codes(env: &Value) -> Vec<String> {
    env["diagnostics"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["code"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}
