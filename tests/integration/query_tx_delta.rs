//! Integration tests for `eg query since` (issue #197).
//!
//! The transaction-time delta feed: a resuming agent (or swarm operator)
//! asks "what entered the store since my last cursor?" and gets every
//! record with `transaction_time` strictly after the cursor, ordered by
//! `(transaction_time, record_id)`, with a `next_cursor` for gap-free
//! resumption.
//!
//! Covered acceptance criteria:
//! - AC1: `eg query since --tx-after <cursor>` returns strictly-after
//!   records spanning all domains, each carrying `domain`, `kind`,
//!   `record_id`, `transaction_time`.
//! - AC2: `next_cursor` round-trips with no gaps and no overlap (resumable).
//! - AC3: window partitions compose: delta(T0,T1) ∪ delta(T1,T2) ==
//!   delta(T0,T2) as sets.
//! - AC4: deterministic `(transaction_time, record_id)` ordering.
//! - AC5: `--domain` filters to one or more domains.
//! - AC6: retraction/supersession events appear as records with `event`
//!   labels.
//! - AC7: empty delta reports `up_to_date`, distinct from `invalid_cursor`.
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    GraphRecord,
    ir::{Graph, NodeKind},
};
use assert_cmd::Command as CargoCommand;
use serde_json::Value;

const SECRET_MARKER: &str = "TOPSECRET_DELTA_BODY_PAYLOAD";

const T0: &str = "2025-12-31T00:00:00Z";
const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("egregore binary should be built")
}

/// Seeds a multi-domain store: a code symbol at T1, an agent observation and
/// a project task at T2, a verification record plus a supersession and a
/// retraction at T3. Every body carries the secret marker so tests can prove
/// the feed never leaks raw record bodies.
fn seed_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("delta-store.jsonl");

    let symbol = GraphRecord::symbol(
        "codegraph:v6:sym-widget".to_owned(),
        "fn",
        "src/lib.rs".to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 40,
            start_line: 1,
            end_line: 5,
            start_column: None,
            end_column: None,
        },
        "widget".to_owned(),
        format!("widget body {SECRET_MARKER}"),
    )
    .with_transaction_time(T1);

    let observation = GraphRecord::node(
        "agent_memory:v1:obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        Some("noted widget shape".to_owned()),
        format!("observation body {SECRET_MARKER}"),
    )
    .with_domain("agent_memory", 1)
    .with_transaction_time(T2);

    let task = GraphRecord::node(
        "project:v1:task-1".to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("ship widget".to_owned()),
        format!("task body {SECRET_MARKER}"),
    )
    .with_domain("project", 1)
    .with_transaction_time(T2);

    let verification = GraphRecord::node(
        "verification:v1:ver-1".to_owned(),
        NodeKind::Verification,
        None,
        None,
        None,
        format!("verification log {SECRET_MARKER}"),
    )
    .with_domain("verification", 1)
    .with_transaction_time(T3);

    // Supersession: the old observation is re-recorded carrying superseded_by.
    let old_obs = GraphRecord::node(
        "agent_memory:v1:obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        Some("noted widget shape".to_owned()),
        format!("observation body {SECRET_MARKER}"),
    )
    .with_domain("agent_memory", 1)
    .with_transaction_time(T3)
    .with_superseded_by("agent_memory:v1:obs-2");

    let retraction = GraphRecord::node(
        "agent_memory:v1:retract-1".to_owned(),
        NodeKind::Retraction,
        None,
        None,
        None,
        format!("retraction note {SECRET_MARKER}"),
    )
    .with_domain("agent_memory", 1)
    .with_transaction_time(T3);

    let mut graph = Graph::new();
    graph.push(symbol);
    graph.push(observation);
    graph.push(task);
    graph.push(verification);
    graph.push(old_obs);
    graph.push(retraction);
    let jsonl = graph.to_jsonl().expect("fixture should serialize");
    fs::write(&path, jsonl).expect("fixture should be written");
    (temp, path)
}

fn run_since(graph: &PathBuf, extra: &[&str]) -> (bool, String, Value) {
    let assert = egregore()
        .args(["query", "since", "--graph"])
        .arg(graph)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    let success = output.status.success();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let value: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (success, stdout, value)
}

fn record_ids(value: &Value) -> Vec<&str> {
    value["records"]
        .as_array()
        .expect("records array")
        .iter()
        .map(|r| r["record_id"].as_str().expect("record_id"))
        .collect()
}

// ── AC1: delta rows carry the minimum payload ────────────────────────────────

#[test]
fn since_returns_strictly_after_records_with_minimum_payload() {
    let (_t, graph) = seed_store();
    let (ok, stdout, value) = run_since(&graph, &["--tx-after", T0]);

    assert!(ok, "query should succeed");
    assert_eq!(value["ok"], true);
    assert_eq!(value["verb"], "since");
    assert_eq!(value["tx_after"], T0);
    // 6 records: symbol, observation, task, verification, superseded obs, retraction.
    assert_eq!(value["records"].as_array().unwrap().len(), 6);
    assert_eq!(value["next_cursor"], T3);
    assert_eq!(value["up_to_date"], false);

    for row in value["records"].as_array().unwrap() {
        assert!(row["domain"].is_string(), "domain required");
        assert!(row["kind"].is_string(), "kind required");
        assert!(row["record_id"].is_string(), "record_id required");
        assert!(
            row["transaction_time"].is_string(),
            "transaction_time required"
        );
    }
    // AC4: deterministic (transaction_time, record_id) order.
    let keys: Vec<(String, String)> = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["transaction_time"].as_str().unwrap().to_owned(),
                r["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        keys, sorted,
        "rows must be ordered by (transaction_time, record_id)"
    );

    // Cursor is exclusive: nothing at exactly T1 is excluded wrongly, and a
    // cursor at T1 drops the T1 symbol.
    let (ok, _, after_t1) = run_since(&graph, &["--tx-after", T1]);
    assert!(ok);
    assert_eq!(after_t1["records"].as_array().unwrap().len(), 5);
    assert!(
        !record_ids(&after_t1).contains(&"codegraph:v6:sym-widget"),
        "cursor is exclusive"
    );

    // Redaction: raw bodies never leak into the feed.
    assert!(
        !stdout.contains(SECRET_MARKER),
        "feed must not emit raw record bodies"
    );
}

// ── AC2: next_cursor round-trips with no gaps and no overlap ────────────────

#[test]
fn since_next_cursor_resumes_without_gaps_or_overlap() {
    let (_t, graph) = seed_store();
    let (ok, _, page1) = run_since(&graph, &["--tx-after", T0]);
    assert!(ok);
    let cursor = page1["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_owned();

    let (ok, _, page2) = run_since(&graph, &["--tx-after", &cursor]);
    assert!(ok);
    assert_eq!(page2["up_to_date"], true, "nothing is newer than the max");
    assert_eq!(
        page2["next_cursor"], cursor,
        "empty delta echoes the cursor"
    );
    assert!(page2["records"].as_array().unwrap().is_empty());

    // A mid-window cursor partitions the feed: no overlap, no gaps.
    let (ok, _, head) = run_since(&graph, &["--tx-after", T0]);
    assert!(ok);
    let mid = head["records"].as_array().unwrap()[2]["transaction_time"]
        .as_str()
        .unwrap()
        .to_owned();
    let (ok, _, left) = run_since(&graph, &["--tx-after", T0]);
    assert!(ok);
    // Use (transaction_time, record_id) pairs: the same record_id can appear
    // at multiple timestamps (supersession), which is not overlap.
    let left_keys: Vec<(String, String)> = left["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["transaction_time"].as_str().unwrap() <= mid.as_str())
        .map(|r| {
            (
                r["transaction_time"].as_str().unwrap().to_owned(),
                r["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let (ok, _, right) = run_since(&graph, &["--tx-after", &mid]);
    assert!(ok);
    let right_keys: Vec<(String, String)> = right["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["transaction_time"].as_str().unwrap().to_owned(),
                r["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        left_keys.iter().all(|k| !right_keys.contains(k)),
        "no overlap across the resume point"
    );
    let mut union: Vec<(String, String)> = left_keys.into_iter().chain(right_keys).collect();
    union.sort();
    union.dedup();
    let mut whole: Vec<(String, String)> = head["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["transaction_time"].as_str().unwrap().to_owned(),
                r["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    whole.sort();
    whole.dedup();
    assert_eq!(union, whole, "partition replays the whole window");
}

// ── AC7: empty delta is up_to_date, invalid cursor errors ───────────────────

#[test]
fn since_empty_delta_is_up_to_date_not_an_error() {
    let (_t, graph) = seed_store();
    let (ok, _, value) = run_since(&graph, &["--tx-after", "2027-01-01T00:00:00Z"]);

    assert!(ok, "empty delta is a well-formed answer");
    assert_eq!(value["ok"], true);
    assert_eq!(value["up_to_date"], true);
    assert_eq!(value["empty_reason"], "up_to_date");
    assert_eq!(value["records"].as_array().unwrap().len(), 0);
    assert_eq!(value["next_cursor"], "2027-01-01T00:00:00Z");
}

#[test]
fn since_invalid_cursor_is_a_machine_readable_error() {
    let (_t, graph) = seed_store();
    let assert = egregore()
        .args(["query", "since", "--graph"])
        .arg(&graph)
        .args(["--tx-after", "not-a-time"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value = serde_json::from_str(stdout.trim()).expect("error envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "invalid_cursor");
}

// ── AC5: domain filters ─────────────────────────────────────────────────────

#[test]
fn since_domain_filter_restricts_to_requested_domains() {
    let (_t, graph) = seed_store();

    let (ok, _, value) = run_since(&graph, &["--tx-after", T0, "--domain", "verification"]);
    assert!(ok);
    let rows = value["records"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["domain"], "verification");

    let (ok, _, value) = run_since(
        &graph,
        &[
            "--tx-after",
            T0,
            "--domain",
            "agent_memory",
            "--domain",
            "project",
        ],
    );
    assert!(ok);
    let rows = value["records"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for row in rows {
        assert!(["agent_memory", "project"].contains(&row["domain"].as_str().unwrap()));
    }
}

#[test]
fn since_unknown_domain_is_a_machine_readable_error() {
    let (_t, graph) = seed_store();
    let assert = egregore()
        .args(["query", "since", "--graph"])
        .arg(&graph)
        .args(["--tx-after", T0, "--domain", "nope"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: Value = serde_json::from_str(stdout.trim()).expect("error envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "unknown_domain");
}

// ── AC6: retraction/supersession appear as records ───────────────────────────

#[test]
fn since_surfaces_retraction_and_supersession_as_records() {
    let (_t, graph) = seed_store();
    let (ok, _, value) = run_since(&graph, &["--tx-after", T0]);
    assert!(ok);
    let rows = value["records"].as_array().unwrap();

    let superseded: Vec<&Value> = rows.iter().filter(|r| r["event"] == "superseded").collect();
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0]["record_id"], "agent_memory:v1:obs-1");
    assert_eq!(superseded[0]["superseded_by"], "agent_memory:v1:obs-2");

    let retracted: Vec<&Value> = rows.iter().filter(|r| r["event"] == "retracted").collect();
    assert_eq!(retracted.len(), 1);
    assert_eq!(retracted[0]["record_id"], "agent_memory:v1:retract-1");

    // Both writes of obs-1 appear: the feed never collapses versions.
    let obs1_count = rows
        .iter()
        .filter(|r| r["record_id"] == "agent_memory:v1:obs-1")
        .count();
    assert_eq!(obs1_count, 2);
}

// ── Limit: truncation cuts between tx groups ────────────────────────────────

#[test]
fn since_limit_truncates_between_tx_groups_and_resumes() {
    let (_t, graph) = seed_store();
    // T2 holds 2 records; limit 1 must still deliver the whole T2 group… but
    // the T1 group comes first (1 record), so limit 1 delivers the T1 group
    // and reports truncation.
    let (ok, _, page1) = run_since(&graph, &["--tx-after", T0, "--limit", "1"]);
    assert!(ok);
    assert_eq!(page1["truncated"], true);
    assert_eq!(page1["total_matching"], 6);
    assert_eq!(page1["records"].as_array().unwrap().len(), 1);
    assert_eq!(page1["next_cursor"], T1);

    let cursor = page1["next_cursor"].as_str().unwrap().to_owned();
    let (ok, _, page2) = run_since(&graph, &["--tx-after", &cursor, "--limit", "100"]);
    assert!(ok);
    assert_eq!(page2["truncated"], false);
    let ids1 = record_ids(&page1);
    let ids2 = record_ids(&page2);
    assert!(ids1.iter().all(|id| !ids2.contains(id)), "no overlap");
    assert_eq!(ids1.len() + ids2.len(), 6, "no gaps");
}

#[test]
fn since_rejects_bad_limits() {
    let (_t, graph) = seed_store();
    for bad in ["0", "5001"] {
        let assert = egregore()
            .args(["query", "since", "--graph"])
            .arg(&graph)
            .args(["--tx-after", T0, "--limit", bad])
            .assert()
            .failure();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
        let value: Value = serde_json::from_str(stdout.trim()).expect("error envelope");
        assert_eq!(value["error"]["code"], "invalid_limit", "limit {bad}");
    }
}

// ── Text format ─────────────────────────────────────────────────────────────

#[test]
fn since_text_format_reports_rows_and_cursor() {
    let (_t, graph) = seed_store();
    let assert = egregore()
        .args(["query", "since", "--graph"])
        .arg(&graph)
        .args(["--tx-after", T0, "--format", "text"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(stdout.contains("# next_cursor: 2026-01-03T00:00:00Z"));
    assert!(stdout.contains("codegraph:v6:sym-widget"));
    assert!(stdout.contains("superseded"));
    assert!(stdout.contains("retracted"));
    assert!(!stdout.contains(SECRET_MARKER));

    let assert = egregore()
        .args(["query", "since", "--graph"])
        .arg(&graph)
        .args(["--tx-after", "2027-01-01T00:00:00Z", "--format", "text"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    assert!(stdout.contains("Up to date"));
}
