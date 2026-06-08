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
    GraphRecord, SourceSpan, TemporalMetadata,
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
fn tx_as_of_malformed_timestamp_is_rejected_before_reading_the_store() {
    // A malformed instant must yield the `invalid_timestamp` envelope even when
    // the local store cannot be read at all, proving selectors are validated
    // before `load_query_records_history` touches the store.
    let missing = std::path::Path::new("/nonexistent/definitely/not/here.jsonl");
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "widget", "--graph"])
        .arg(missing)
        .args(["--tx-as-of", "not-a-timestamp"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let env: Value = serde_json::from_str(stdout.trim()).expect("JSON envelope on stdout");
    assert_eq!(env["ok"], false);
    assert_eq!(
        env["error"]["code"], "invalid_timestamp",
        "must reject the timestamp before failing to read the missing store"
    );
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
    let result =
        symbol_as_of_transaction_time(&records, "widget", "2026-01-02T00:00:00Z", None, None)
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

    let result =
        symbol_as_of_transaction_time(&records, "widget", "2026-01-02T00:00:00Z", None, None)
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

// ── Ordering: multiple matching symbols sort by (span.start_line, record_id) ──
// Matches the daemon `symbol_by_name` contract so a `max_results` truncation
// keeps the documented prefix.

#[test]
fn tx_as_of_orders_multiple_symbols_by_span_then_id() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // Two distinct symbols both named "widget", at different start lines.
    let later_line = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/b.rs", "widget"]),
        "fn",
        "src/b.rs".to_owned(),
        span(40, 45),
        "widget".to_owned(),
        "widget b".to_owned(),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX);
    let earlier_line = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/a.rs", "widget"]),
        "fn",
        "src/a.rs".to_owned(),
        span(10, 15),
        "widget".to_owned(),
        "widget a".to_owned(),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX);
    // Push in reverse to prove the sort, not insertion order, decides output.
    let records = vec![later_line, earlier_line];

    let result =
        symbol_as_of_transaction_time(&records, "widget", "2026-02-01T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(result.records.len(), 2);
    let lines: Vec<usize> = result
        .records
        .iter()
        .map(|r| match r {
            GraphRecord::Node { span: Some(s), .. } => s.start_line,
            _ => 0,
        })
        .collect();
    assert_eq!(lines, vec![10, 40], "rows must sort by span.start_line");
}

// ── Envelope: `as_of` is always present (null when no valid-time axis) ───────

#[test]
fn tx_as_of_envelope_includes_null_as_of_when_absent() {
    let (_t, graph) = seed_store();
    let (_ok, env) = run_tx_query(&graph, &["--tx-as-of", "2026-01-02T00:00:00Z"]);
    assert!(
        env.get("as_of").is_some(),
        "as_of key must be present even without --as-of"
    );
    assert!(
        env["as_of"].is_null(),
        "as_of must serialize as null when no valid-time axis is supplied"
    );
}

// ── AC2: cross-id supersession excluded once the replacement is known ────────

#[test]
fn tx_as_of_excludes_superseded_symbol_once_replacement_known() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let new_id = stable_id(&["node", "Symbol", "src/new.rs", "widget"]);
    let old = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/old.rs", "widget"]),
        "fn",
        "src/old.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        "widget old".to_owned(),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX)
    .with_superseded_by(new_id.clone());
    let new = GraphRecord::symbol(
        new_id,
        "fn",
        "src/new.rs".to_owned(),
        span(1, 6),
        "widget".to_owned(),
        "widget new".to_owned(),
    )
    .with_node_time(V2_VT, "author_provided", V2_TX)
    .with_transaction_time(V2_TX);
    let records = vec![old, new];

    // Before the replacement is committed: supersession isn't known yet → keep old.
    let before =
        symbol_as_of_transaction_time(&records, "widget", "2026-01-02T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(before.records.len(), 1, "only the old symbol is known yet");
    assert_eq!(record_path(before.records[0]), Some("src/old.rs"));

    // After the replacement is committed: the superseded old row is dropped.
    let after =
        symbol_as_of_transaction_time(&records, "widget", "2026-01-04T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(
        after.records.len(),
        1,
        "superseded old row must be excluded"
    );
    assert_eq!(record_path(after.records[0]), Some("src/new.rs"));
    assert!(
        after.diagnostics.iter().any(|d| d.code == "superseded"),
        "exclusion must be reported via a `superseded` diagnostic"
    );
}

// ── AC4 + AC2: supersession must respect the valid-time axis ─────────────────
// A two-axis query asks "what was true at valid time V, as known by tx time T."
// If the replacement is known by T but only becomes valid AFTER V, the older
// row that was true at V must NOT be dropped as superseded.

#[test]
fn tx_as_of_supersession_respects_valid_time_axis() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let new_id = stable_id(&["node", "Symbol", "src/new.rs", "widget"]);
    // Old: valid + committed at 2026-01-01.
    let old = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/old.rs", "widget"]),
        "fn",
        "src/old.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        "widget old".to_owned(),
    )
    .with_node_time(
        "2026-01-01T00:00:00Z",
        "author_provided",
        "2026-01-01T00:00:00Z",
    )
    .with_transaction_time("2026-01-01T00:00:00Z")
    .with_superseded_by(new_id.clone());
    // New: committed at 2026-01-03 (known by T) but only valid from 2026-01-03.
    let new = GraphRecord::symbol(
        new_id,
        "fn",
        "src/new.rs".to_owned(),
        span(1, 6),
        "widget".to_owned(),
        "widget new".to_owned(),
    )
    .with_node_time(
        "2026-01-03T00:00:00Z",
        "author_provided",
        "2026-01-03T00:00:00Z",
    )
    .with_transaction_time("2026-01-03T00:00:00Z");
    let records = vec![old, new];

    // Known by 2026-01-04, but asking what was true at valid-time 2026-01-02:
    // the replacement is not yet valid then, so the old row must survive.
    let result = symbol_as_of_transaction_time(
        &records,
        "widget",
        "2026-01-04T00:00:00Z",
        Some("2026-01-02T00:00:00Z"),
        None,
    )
    .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "old row valid at V must not be dropped when the replacement isn't valid yet"
    );
    assert_eq!(record_path(result.records[0]), Some("src/old.rs"));
}

// ── AC2: rename-style supersession (replacement has a different name) ────────

#[test]
fn tx_as_of_excludes_superseded_symbol_renamed_replacement() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // old_widget is renamed to new_widget: the replacement carries a different
    // name, so it never matches a query for "old_widget" — but once it is
    // effective by the instant, the stale old-name row must still be dropped.
    let new_id = stable_id(&["node", "Symbol", "src/lib.rs", "new_widget"]);
    let old = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "old_widget"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "old_widget".to_owned(),
        "old".to_owned(),
    )
    .with_node_time(V1_VT, "author_provided", V1_TX)
    .with_transaction_time(V1_TX)
    .with_superseded_by(new_id.clone());
    let new = GraphRecord::symbol(
        new_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "new_widget".to_owned(),
        "new".to_owned(),
    )
    .with_node_time(V2_VT, "author_provided", V2_TX)
    .with_transaction_time(V2_TX);
    let records = vec![old, new];

    // After the rename is known, querying the old name yields an empty view plus
    // a superseded diagnostic — not the stale row.
    let after =
        symbol_as_of_transaction_time(&records, "old_widget", "2026-01-04T00:00:00Z", None, None)
            .expect("query ok");
    assert!(
        after.records.is_empty(),
        "renamed-away symbol must not be returned once the replacement is effective"
    );
    assert!(
        after.diagnostics.iter().any(|d| d.code == "superseded"),
        "must report the supersession, got {:?}",
        after.diagnostics
    );

    // Before the rename is known, the old name is still the live view.
    let before =
        symbol_as_of_transaction_time(&records, "old_widget", "2026-01-02T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(
        before.records.len(),
        1,
        "old name is live before the rename"
    );
}

// ── AC6: in-range absence (symbol introduced later) is distinct from
//        out-of-range (before the whole store).

#[test]
fn tx_as_of_symbol_introduced_later_reports_not_yet_known() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // Store first transaction is 2026-01-01 (early), the queried symbol first
    // appears at 2026-01-03. Querying at 2026-01-02 is IN the store range but
    // before the symbol existed → symbol_not_yet_known, not before_first.
    let early = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/a.rs", "early"]),
        "fn",
        "src/a.rs".to_owned(),
        span(1, 5),
        "early".to_owned(),
        "early".to_owned(),
    )
    .with_node_time(
        "2026-01-01T00:00:00Z",
        "author_provided",
        "2026-01-01T00:00:00Z",
    )
    .with_transaction_time("2026-01-01T00:00:00Z");
    let latecomer = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/b.rs", "latecomer"]),
        "fn",
        "src/b.rs".to_owned(),
        span(1, 5),
        "latecomer".to_owned(),
        "late".to_owned(),
    )
    .with_node_time(
        "2026-01-03T00:00:00Z",
        "author_provided",
        "2026-01-03T00:00:00Z",
    )
    .with_transaction_time("2026-01-03T00:00:00Z");
    let records = vec![early, latecomer];

    let result =
        symbol_as_of_transaction_time(&records, "latecomer", "2026-01-02T00:00:00Z", None, None)
            .expect("query ok");
    assert!(result.records.is_empty());
    let codes: Vec<&str> = result.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"symbol_not_yet_known"),
        "in-range absence must report symbol_not_yet_known, got {codes:?}"
    );
    assert!(
        !codes.contains(&"before_first_transaction"),
        "must not claim out-of-range when the instant is within the store range, got {codes:?}"
    );
}

// ── AC2: history-replay removal — a symbol removed at a later commit must not be
//        carried forward to `--tx-as-of` instants after the removal.

#[test]
fn tx_as_of_excludes_removed_history_symbol() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // gone: present at commit c1 (01-01) and c2 (01-02), removed at c3.
    // kept: present at c1, c2, AND c3 (01-03) — proves c3 is the active commit.
    let gone_id = stable_id(&["node", "Symbol", "src/lib.rs", "gone"]);
    let kept_id = stable_id(&["node", "Symbol", "src/lib.rs", "kept"]);
    let mut records = Vec::new();
    // Linear history c1 ← c2 ← c3 (parent links scope the repository).
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &gone_id,
            "gone",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
        ("c3c3c3c3", &["c2c2c2c2"][..], "2026-01-03T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }

    // At c2 (before removal) `gone` is still live.
    let at_c2 = symbol_as_of_transaction_time(&records, "gone", "2026-01-02T12:00:00Z", None, None)
        .expect("query ok");
    assert_eq!(at_c2.records.len(), 1, "gone is present at c2");

    // After c3 (its removal commit) `gone` must be excluded with a diagnostic.
    let after_c3 =
        symbol_as_of_transaction_time(&records, "gone", "2026-01-04T00:00:00Z", None, None)
            .expect("query ok");
    assert!(
        after_c3.records.is_empty(),
        "removed history symbol must not be carried past its removal"
    );
    assert!(
        after_c3
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "removal must be reported, got {:?}",
        after_c3.diagnostics
    );

    // `kept` (present at c3) is still returned after c3.
    let kept_after =
        symbol_as_of_transaction_time(&records, "kept", "2026-01-04T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(kept_after.records.len(), 1, "kept survives at c3");
}

// ── AC4: history removal must not erase an earlier valid-time answer ─────────
// A two-axis query asks what was true at valid time V; a row true at V must not
// be dropped just because the symbol was removed at a later commit known by T.

#[test]
fn tx_as_of_history_removal_respects_valid_time_axis() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let gone_id = stable_id(&["node", "Symbol", "src/lib.rs", "gone"]);
    let kept_id = stable_id(&["node", "Symbol", "src/lib.rs", "kept"]);
    let mut records = Vec::new();
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &gone_id,
            "gone",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
        ("c3c3c3c3", &["c2c2c2c2"][..], "2026-01-03T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }

    // Known by 2026-01-04 (after the removal commit c3), but asking what was TRUE
    // at valid time 2026-01-01 → `gone` existed then, so it must still be returned.
    let result = symbol_as_of_transaction_time(
        &records,
        "gone",
        "2026-01-04T00:00:00Z",
        Some("2026-01-01T12:00:00Z"),
        None,
    )
    .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "valid-time axis asks what was true at V; a later removal must not erase it"
    );
}

// ── AC4: two-axis removal — valid time AFTER the removal must drop the row ─────
// The mirror of the case above: when the requested valid time is after the
// removal commit, the symbol was genuinely absent at V, so it must be excluded
// rather than carried forward from its last pre-removal snapshot.

#[test]
fn tx_as_of_history_removal_two_axis_valid_after_removal_excludes() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let gone_id = stable_id(&["node", "Symbol", "src/lib.rs", "gone"]);
    let kept_id = stable_id(&["node", "Symbol", "src/lib.rs", "kept"]);
    let mut records = Vec::new();
    // gone present at c1/c2, removed at c3; kept survives to c3 (marks c3 active).
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &gone_id,
            "gone",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
        ("c3c3c3c3", &["c2c2c2c2"][..], "2026-01-03T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            commit,
            parents,
            date,
        ));
    }

    // Known by 2026-01-04 AND asking what was true at valid time 2026-01-03T12
    // (after the removal commit c3) → `gone` was absent then, so excluded.
    let result = symbol_as_of_transaction_time(
        &records,
        "gone",
        "2026-01-04T00:00:00Z",
        Some("2026-01-03T12:00:00Z"),
        None,
    )
    .expect("query ok");
    assert!(
        result.records.is_empty(),
        "a valid time after the removal must not carry the pre-removal snapshot, got {:?}",
        result.records
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "removal in the requested valid-time view must be reported, got {:?}",
        result.diagnostics
    );

    // Control: `kept` (present at c3) is still returned at the same instants.
    let kept = symbol_as_of_transaction_time(
        &records,
        "kept",
        "2026-01-04T00:00:00Z",
        Some("2026-01-03T12:00:00Z"),
        None,
    )
    .expect("query ok");
    assert_eq!(kept.records.len(), 1, "kept is live at c3 and valid at V");
}

// ── Same-second commits: removal must still be detected (#1564) ──────────────
// Git commit timestamps are second-resolution; a removal commit can share its
// second with the prior commit. Commit topology (parent links) must break the
// tie so the later commit still counts as active.

#[test]
fn tx_as_of_detects_removal_across_same_second_commits() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let gone_id = stable_id(&["node", "Symbol", "src/lib.rs", "gone"]);
    let kept_id = stable_id(&["node", "Symbol", "src/lib.rs", "kept"]);
    // c2 (last commit holding `gone`) and c3 (its removal commit) share a second.
    let same_second = "2026-01-02T00:00:00Z";
    let mut records = vec![
        history_symbol(
            &gone_id,
            "gone",
            "src/lib.rs",
            "c1c1c1c1",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &gone_id,
            "gone",
            "src/lib.rs",
            "c2c2c2c2",
            &["c1c1c1c1"],
            same_second,
        ),
        history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            "c1c1c1c1",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            "c2c2c2c2",
            &["c1c1c1c1"],
            same_second,
        ),
        history_symbol(
            &kept_id,
            "kept",
            "src/lib.rs",
            "c3c3c3c3",
            &["c2c2c2c2"],
            same_second,
        ),
    ];
    records.reverse(); // order independence

    let after = symbol_as_of_transaction_time(&records, "gone", "2026-01-02T00:00:01Z", None, None)
        .expect("query ok");
    assert!(
        after.records.is_empty(),
        "removal at a same-second child commit must still be detected, got {:?}",
        after.records.len()
    );
    assert!(
        after
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "same-second removal must be reported"
    );
}

// ── Multi-repository store: scope removal to the symbol's repo (#1562) ────────
// A later commit in a different repository sharing the store must not make a
// symbol from another repository look removed.

#[test]
fn tx_as_of_scopes_removal_to_symbol_repository() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let alpha = stable_id(&["node", "Symbol", "a/lib.rs", "alpha"]);
    let beta = stable_id(&["node", "Symbol", "b/lib.rs", "beta"]);
    // Repo A: a1 ← a2, alpha lives through a2 (latest 01-02).
    // Repo B: b1 ← b2, a later commit b2 at 01-05 — disjoint commit graph.
    let records = vec![
        history_symbol(
            &alpha,
            "alpha",
            "a/lib.rs",
            "a1a1a1a1",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &alpha,
            "alpha",
            "a/lib.rs",
            "a2a2a2a2",
            &["a1a1a1a1"],
            "2026-01-02T00:00:00Z",
        ),
        history_symbol(
            &beta,
            "beta",
            "b/lib.rs",
            "b1b1b1b1",
            &[],
            "2026-01-04T00:00:00Z",
        ),
        history_symbol(
            &beta,
            "beta",
            "b/lib.rs",
            "b2b2b2b2",
            &["b1b1b1b1"],
            "2026-01-05T00:00:00Z",
        ),
    ];

    // Query alpha after repo B's latest commit: alpha's own repo has no newer
    // commit, so it must NOT be reported as removed.
    let result =
        symbol_as_of_transaction_time(&records, "alpha", "2026-01-06T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "a later commit in another repository must not remove this symbol, got diagnostics {:?}",
        result.diagnostics
    );
}

// ── Equal transaction-time tie-break prefers the later version (#1410) ────────

#[test]
fn tx_as_of_equal_transaction_time_prefers_later_commit() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let id = stable_id(&["node", "Symbol", "src/lib.rs", "widget"]);
    // Two versions of the same symbol committed in the same second; c2 is c1's
    // child, so it is the later (correct) version.
    let same_second = "2026-01-02T00:00:00Z";
    let records = vec![
        history_symbol(&id, "widget", "src/lib.rs", "c1c1c1c1", &[], same_second),
        history_symbol(
            &id,
            "widget",
            "src/lib.rs",
            "c2c2c2c2",
            &["c1c1c1c1"],
            same_second,
        ),
    ];

    let result =
        symbol_as_of_transaction_time(&records, "widget", "2026-01-03T00:00:00Z", None, None)
            .expect("query ok");
    assert_eq!(result.records.len(), 1);
    let commit = match result.records[0] {
        GraphRecord::Node {
            temporal: Some(t), ..
        } => t.git_commit.as_str(),
        _ => panic!("expected a temporal node"),
    };
    assert_eq!(
        commit, "c2c2c2c2",
        "equal-transaction-time tie must resolve to the later (child) commit"
    );
}

// ── Multi-repository store: same name, removal scoped per repo component ───────
// When two repositories in one store both define a symbol of the same name, a
// removal in one repository must not drop the still-live row in the other.

#[test]
fn tx_as_of_same_name_removal_does_not_drop_other_repository() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let foo_a = stable_id(&["node", "Symbol", "a/lib.rs", "foo"]);
    let keep_a = stable_id(&["node", "Symbol", "a/lib.rs", "keep"]);
    let foo_b = stable_id(&["node", "Symbol", "b/lib.rs", "foo"]);
    // Repo A: a1 ← a2 ← a3. `foo` lives through a2 then is removed at a3, while
    // `keep` survives to a3 (so a3 is the active commit of repo A's component).
    // Repo B: b1 ← b2, where `foo` is still present at the tip — a disjoint graph.
    let records = vec![
        history_symbol(
            &foo_a,
            "foo",
            "a/lib.rs",
            "a1a1a1a1",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &foo_a,
            "foo",
            "a/lib.rs",
            "a2a2a2a2",
            &["a1a1a1a1"],
            "2026-01-02T00:00:00Z",
        ),
        history_symbol(
            &keep_a,
            "keep",
            "a/lib.rs",
            "a1a1a1a1",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &keep_a,
            "keep",
            "a/lib.rs",
            "a2a2a2a2",
            &["a1a1a1a1"],
            "2026-01-02T00:00:00Z",
        ),
        history_symbol(
            &keep_a,
            "keep",
            "a/lib.rs",
            "a3a3a3a3",
            &["a2a2a2a2"],
            "2026-01-03T00:00:00Z",
        ),
        history_symbol(
            &foo_b,
            "foo",
            "b/lib.rs",
            "b1b1b1b1",
            &[],
            "2026-01-04T00:00:00Z",
        ),
        history_symbol(
            &foo_b,
            "foo",
            "b/lib.rs",
            "b2b2b2b2",
            &["b1b1b1b1"],
            "2026-01-05T00:00:00Z",
        ),
    ];

    // Query `foo` after every commit: repo A removed it (a2 < active a3) but
    // repo B still defines it, so exactly repo B's row survives.
    let result = symbol_as_of_transaction_time(&records, "foo", "2026-01-06T00:00:00Z", None, None)
        .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "repo B's live `foo` must survive repo A's removal, got diagnostics {:?}",
        result.diagnostics
    );
    assert_eq!(
        record_path(result.records[0]),
        Some("b/lib.rs"),
        "the surviving row must be repo B's, not repo A's removed one"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "repo A's removal must still be reported, got {:?}",
        result.diagnostics
    );
}

// ── Same name, same repo: removal is pruned per stable ID, not per name ────────
// One repository can hold several distinct `Symbol`s sharing a name (one per
// file). Removing one must not be masked by another that survives at the active
// commit — the removed stable ID is excluded while the live one is kept.

#[test]
fn tx_as_of_same_name_same_repo_prunes_only_the_removed_stable_id() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // Both `foo`s live in one repository (shared linear history c1←c2←c3).
    let foo_a = stable_id(&["node", "Symbol", "src/a.rs", "foo"]);
    let foo_b = stable_id(&["node", "Symbol", "src/b.rs", "foo"]);
    let mut records = Vec::new();
    // src/a.rs::foo present at c1/c2, removed at c3.
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &foo_a, "foo", "src/a.rs", commit, parents, date,
        ));
    }
    // src/b.rs::foo present at c1/c2/c3 (so c3 is the active commit, and b's foo
    // is still live there).
    for (commit, parents, date) in [
        ("c1c1c1c1", &[][..], "2026-01-01T00:00:00Z"),
        ("c2c2c2c2", &["c1c1c1c1"][..], "2026-01-02T00:00:00Z"),
        ("c3c3c3c3", &["c2c2c2c2"][..], "2026-01-03T00:00:00Z"),
    ] {
        records.push(history_symbol(
            &foo_b, "foo", "src/b.rs", commit, parents, date,
        ));
    }

    // After c3: a's foo was removed; b's foo (live at the active commit) survives.
    let result = symbol_as_of_transaction_time(&records, "foo", "2026-01-04T00:00:00Z", None, None)
        .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "a surviving same-name symbol must not mask the removed one, got {:?}",
        result
            .records
            .iter()
            .map(|r| record_path(r))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        record_path(result.records[0]),
        Some("src/b.rs"),
        "the live `foo` (src/b.rs) is kept; the removed `foo` (src/a.rs) is excluded"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "the removed stable ID must be reported, got {:?}",
        result.diagnostics
    );
}

// ── Forks sharing an ancestor: a sibling-branch commit is not a descendant ─────
// Two forks that share Git ancestry land in one connected component, but a later
// commit on one fork is not a descendant of a live symbol on the other and must
// not make it look removed.

#[test]
fn tx_as_of_fork_sharing_ancestor_keeps_live_symbol() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    // Shared ancestor c0; two forks diverge from it. `foo` lives on fork B
    // (c0 ← b1) and is still present at its tip b1. Fork A (c0 ← a1) has a later
    // commit but never contained `foo`. c0 connects both forks into one component,
    // yet a1 is NOT a descendant of b1.
    let foo = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
    let marker = stable_id(&["node", "Symbol", "src/other.rs", "marker"]);
    let records = vec![
        history_symbol(
            &foo,
            "foo",
            "src/lib.rs",
            "c0c0c0c0",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &foo,
            "foo",
            "src/lib.rs",
            "b1b1b1b1",
            &["c0c0c0c0"],
            "2026-01-02T00:00:00Z",
        ),
        // Fork A: a later commit on a sibling branch carrying a different symbol.
        history_symbol(
            &marker,
            "marker",
            "src/other.rs",
            "c0c0c0c0",
            &[],
            "2026-01-01T00:00:00Z",
        ),
        history_symbol(
            &marker,
            "marker",
            "src/other.rs",
            "a1a1a1a1",
            &["c0c0c0c0"],
            "2026-01-05T00:00:00Z",
        ),
    ];

    // After fork A's later commit a1: `foo` on fork B is still live (a1 is not a
    // descendant of b1), so it must not be reported as removed.
    let result = symbol_as_of_transaction_time(&records, "foo", "2026-01-06T00:00:00Z", None, None)
        .expect("query ok");
    assert_eq!(
        result.records.len(),
        1,
        "a later commit on a sibling fork must not remove a live symbol, got diagnostics {:?}",
        result.diagnostics
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code == "absent_at_transaction"),
        "the live fork-B symbol must not be reported as removed, got {:?}",
        result.diagnostics
    );
}

// ── Unknown symbol still reports out-of-range diagnostics ─────────────────────
// A query for a name with no Symbol rows must still distinguish an out-of-range
// temporal instant from a genuine in-range absence.

#[test]
fn tx_as_of_unknown_symbol_before_store_reports_before_first() {
    use aletheia_egregore::query::symbol_as_of_transaction_time;

    let known = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "known"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "known".to_owned(),
        "known".to_owned(),
    )
    .with_node_time(
        "2026-01-02T00:00:00Z",
        "author_provided",
        "2026-01-02T00:00:00Z",
    )
    .with_transaction_time("2026-01-02T00:00:00Z");
    let records = vec![known];

    // Unknown name, instant before the store: both diagnostics, empty view.
    let before =
        symbol_as_of_transaction_time(&records, "ghost", "2025-01-01T00:00:00Z", None, None)
            .expect("query ok");
    assert!(before.records.is_empty());
    let codes: Vec<&str> = before.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"no_named_symbol"),
        "unknown name must report no_named_symbol, got {codes:?}"
    );
    assert!(
        codes.contains(&"before_first_transaction"),
        "an out-of-range instant must still report before_first_transaction even for an unknown name, got {codes:?}"
    );

    // Unknown name, instant in range: only no_named_symbol (a genuine absence).
    let in_range =
        symbol_as_of_transaction_time(&records, "ghost", "2026-02-01T00:00:00Z", None, None)
            .expect("query ok");
    let codes: Vec<&str> = in_range
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert!(
        codes.contains(&"no_named_symbol") && !codes.contains(&"before_first_transaction"),
        "an in-range unknown name must not claim out-of-range, got {codes:?}"
    );
}

// ── Explicit store-wide bounds keep range checks store-wide ───────────────────
// When `records` is a domain-filtered slice of a larger store, passing the
// store-wide bounds must suppress a spurious before_first_transaction.

#[test]
fn tx_as_of_explicit_store_bounds_override_filtered_range() {
    use aletheia_egregore::query::{store_transaction_bounds, symbol_as_of_transaction_time};

    // Domain-filtered slice: the only record is a symbol committed at 2026-03-01.
    let widget = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "widget"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "widget".to_owned(),
        "widget".to_owned(),
    )
    .with_node_time(
        "2026-03-01T00:00:00Z",
        "author_provided",
        "2026-03-01T00:00:00Z",
    )
    .with_transaction_time("2026-03-01T00:00:00Z");
    let slice = vec![widget];

    // The full store also holds an earlier (non-codegraph) transaction at 01-01.
    let early = GraphRecord::node(
        "agent_memory:v1:obs-early".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "early observation".to_owned(),
    )
    .with_domain("agent_memory", 1)
    .with_transaction_time("2026-01-01T00:00:00Z");
    let full_store = vec![slice[0].clone(), early];
    let store_bounds = store_transaction_bounds(&full_store);
    assert!(store_bounds.is_some(), "full store has a transaction range");

    // Querying the slice at 2026-02-01 *without* bounds sees only the 03-01
    // record, so it wrongly reports out-of-range.
    let derived =
        symbol_as_of_transaction_time(&slice, "widget", "2026-02-01T00:00:00Z", None, None)
            .expect("query ok");
    let derived_codes: Vec<&str> = derived
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert!(
        derived_codes.contains(&"before_first_transaction"),
        "the filtered slice alone treats 02-01 as out-of-range, got {derived_codes:?}"
    );

    // With the store-wide bounds, 02-01 is in range — the symbol just did not
    // exist yet, so we get symbol_not_yet_known and never before_first.
    let scoped =
        symbol_as_of_transaction_time(&slice, "widget", "2026-02-01T00:00:00Z", None, store_bounds)
            .expect("query ok");
    let scoped_codes: Vec<&str> = scoped.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        !scoped_codes.contains(&"before_first_transaction"),
        "store-wide bounds must suppress the spurious out-of-range diagnostic, got {scoped_codes:?}"
    );
    assert!(
        scoped_codes.contains(&"symbol_not_yet_known"),
        "an in-range instant before the symbol exists is symbol_not_yet_known, got {scoped_codes:?}"
    );
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn history_symbol(
    id: &str,
    name: &str,
    path: &str,
    commit: &str,
    parents: &[&str],
    date: &str,
) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "fn",
        path.to_owned(),
        span(1, 5),
        name.to_owned(),
        format!("{name} at {commit}"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
        valid_time: date.to_owned(),
        author_time: None,
        observed_at: date.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

fn record_path(r: &GraphRecord) -> Option<&str> {
    match r {
        GraphRecord::Node {
            repo_relative_path, ..
        } => repo_relative_path.as_deref(),
        _ => None,
    }
}

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
