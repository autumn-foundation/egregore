#![allow(missing_docs)]

//! Integration tests for `eg query trust-audit` (issue #236).
//!
//! The lane is a read-only audit: it scans every persisted node, edge, and
//! tombstone and checks that each record's `producer_kind` is consistent with
//! the trust class its node kind implies. Deterministic code-fact kinds may
//! only be written by deterministic producers; agent-authored kinds may only
//! be written by agent producers.
//!
//! - `trust_audit_flags_exactly_the_seeded_violations`: a fixture with one
//!   deliberate violation of each rule shape (node, edge, tombstone, plus an
//!   unclassifiable `other` producer) is flagged with exactly the seeded
//!   records — no more, no fewer — and each violation carries the citable
//!   record fields.
//! - `trust_audit_clean_store_is_explicit_ok`: a clean fixture returns an
//!   explicit `{"ok":true,"violations":[]}` with counted totals.
//! - `trust_audit_is_byte_identical_across_runs`: three runs, byte-identical
//!   stdout (issue #199 determinism contract).
//! - `trust_audit_is_read_only`: the input store is byte-identical before and
//!   after the audit; zero records are created, modified, or deleted.

use std::{collections::BTreeMap, fs};

use aletheia_egregore::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, Producer, ProducerKind,
    SourceSpan,
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn producer(kind: ProducerKind) -> Producer {
    Producer {
        egregore_version: "0.0.0-trust-audit-fixture".to_owned(),
        egregore_git: None,
        producer_kind: kind,
        producer_components: BTreeMap::new(),
        producer_started_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

fn stamped(record: GraphRecord, kind: ProducerKind) -> GraphRecord {
    record.with_producer(producer(kind))
}

/// Code-graph node: `codegraph` domain at the current schema version so the
/// JSONL loader accepts the fixture.
fn codegraph(mut record: GraphRecord) -> GraphRecord {
    if let GraphRecord::Node {
        domain: ref mut d, ..
    } = record
    {
        *d = Some("codegraph".to_owned());
    }
    record
}

/// Agent-memory node: `agent_memory` domain at its schema version.
fn agent_memory(mut record: GraphRecord) -> GraphRecord {
    if let GraphRecord::Node {
        domain: ref mut d,
        schema_version: ref mut v,
        ..
    } = record
    {
        *d = Some("agent_memory".to_owned());
        *v = AGENT_MEMORY_SCHEMA_VERSION;
    }
    record
}

const fn fixture_span() -> SourceSpan {
    SourceSpan {
        start_byte: 10,
        end_byte: 42,
        start_line: 3,
        end_line: 3,
        start_column: None,
        end_column: None,
    }
}

fn tombstone(id: &str, deleted_id: &str, kind: ProducerKind) -> GraphRecord {
    let record = GraphRecord::Tombstone {
        id: id.to_owned(),
        schema_version: aletheia_egregore::ir::SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: "fixture tombstone".to_owned(),
        producer: None,
    };
    stamped(record, kind)
}

// ---------------------------------------------------------------------------
// Fixture: one deliberate violation of each rule shape + clean records.
// ---------------------------------------------------------------------------

/// Returns `(records, expected_violations)`. Each expected violation is
/// `(record_id, kind, producer_kind, rule)`.
type ExpectedViolation = (String, String, String, String);

/// The five deliberately-seeded violations, one per rule shape.
fn seeded_violations() -> (Vec<GraphRecord>, Vec<ExpectedViolation>) {
    let mut records = Vec::new();
    let mut expected = Vec::new();

    // 1. Agent producer under a code-fact kind (the issue's headline case):
    // an `observation_writer`-stamped `Symbol` node.
    let sym_violation = stamped(
        codegraph(GraphRecord::node(
            "trust-audit:sym:agent-produced".to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(fixture_span()),
            Some("planted_symbol".to_owned()),
            "fixture symbol".to_owned(),
        )),
        ProducerKind::ObservationWriter,
    );
    expected.push((
        "trust-audit:sym:agent-produced".to_owned(),
        "Symbol".to_owned(),
        "observation_writer".to_owned(),
        "code_fact_written_by_agent_producer".to_owned(),
    ));
    records.push(sym_violation);

    // 2. Deterministic producer under an agent-authored kind: a
    // `code_graph_extractor`-stamped `Observation` node.
    let obs_violation = stamped(
        agent_memory(GraphRecord::node(
            "trust-audit:obs:extractor-produced".to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "fixture observation".to_owned(),
        )),
        ProducerKind::CodeGraphExtractor,
    );
    expected.push((
        "trust-audit:obs:extractor-produced".to_owned(),
        "Observation".to_owned(),
        "code_graph_extractor".to_owned(),
        "agent_authored_written_by_deterministic_producer".to_owned(),
    ));
    records.push(obs_violation);

    // 3. Unclassifiable producer (`other`): fail closed, never silently
    // accepted.
    let other_violation = stamped(
        codegraph(GraphRecord::node(
            "trust-audit:sym:unknown-producer".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            None,
            "fixture symbol".to_owned(),
        )),
        ProducerKind::Other,
    );
    expected.push((
        "trust-audit:sym:unknown-producer".to_owned(),
        "Symbol".to_owned(),
        "other".to_owned(),
        "unclassifiable_producer_kind".to_owned(),
    ));
    records.push(other_violation);

    // 4. Edge audited against its SOURCE node's kind: a `task_writer`-stamped
    // `CONTAINS` edge out of a code-fact `Symbol`.
    let edge_source = stamped(
        codegraph(GraphRecord::node(
            "trust-audit:sym:edge-source".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            None,
            "fixture symbol".to_owned(),
        )),
        ProducerKind::CodeGraphExtractor,
    );
    records.push(edge_source);
    let edge_violation = stamped(
        GraphRecord::edge(
            EdgeLabel::Contains,
            "trust-audit:sym:edge-source".to_owned(),
            "trust-audit:sym:edge-target".to_owned(),
            None,
            "fixture edge".to_owned(),
        ),
        ProducerKind::TaskWriter,
    );
    let edge_id = edge_violation.id().to_owned();
    expected.push((
        edge_id,
        "CONTAINS".to_owned(),
        "task_writer".to_owned(),
        "code_fact_written_by_agent_producer".to_owned(),
    ));
    records.push(edge_violation);

    // 5. Tombstone audited against its target's kind: an
    // `observation_writer`-stamped tombstone for a code-fact `Symbol`.
    let tomb_violation = tombstone(
        "trust-audit:tombstone:1",
        "trust-audit:sym:agent-produced",
        ProducerKind::ObservationWriter,
    );
    expected.push((
        "trust-audit:tombstone:1".to_owned(),
        "Symbol".to_owned(),
        "observation_writer".to_owned(),
        "code_fact_written_by_agent_producer".to_owned(),
    ));
    records.push(tomb_violation);

    (records, expected)
}

/// Clean, legacy, and unclassifiable records that must stay silent: consistent
/// producer classes, legacy records without a producer envelope, and an edge
/// whose source node is absent from the store.
fn clean_fixture_records() -> Vec<GraphRecord> {
    vec![
        // --- Clean records: consistent producer classes (must stay silent).
        stamped(
            codegraph(GraphRecord::node(
                "trust-audit:sym:clean".to_owned(),
                NodeKind::Symbol,
                Some("src/clean.rs".to_owned()),
                None,
                Some("clean_symbol".to_owned()),
                "fixture symbol".to_owned(),
            )),
            ProducerKind::CodeGraphExtractor,
        ),
        stamped(
            agent_memory(GraphRecord::node(
                "trust-audit:obs:clean".to_owned(),
                NodeKind::Observation,
                None,
                None,
                None,
                "fixture observation".to_owned(),
            )),
            ProducerKind::ObservationWriter,
        ),
        // History replay is a deterministic producer too.
        stamped(
            codegraph(GraphRecord::node(
                "trust-audit:commit:clean".to_owned(),
                NodeKind::Commit,
                None,
                None,
                None,
                "fixture commit".to_owned(),
            )),
            ProducerKind::HistoryReplay,
        ),
        // Legacy records (no producer envelope) are out of scope: they cannot
        // be retroactively known to come from a specific producer, so they are
        // exempt — never violations.
        codegraph(GraphRecord::node(
            "trust-audit:sym:legacy".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            None,
            "fixture symbol".to_owned(),
        )),
        // An edge whose source node is absent from the store cannot be
        // classified against a node kind; it is not a trust-class violation
        // (referential integrity is `eg validate`'s job).
        stamped(
            GraphRecord::edge(
                EdgeLabel::Mentions,
                "trust-audit:missing:source".to_owned(),
                "trust-audit:missing:target".to_owned(),
                None,
                "fixture edge".to_owned(),
            ),
            ProducerKind::ObservationWriter,
        ),
    ]
}

/// Full fixture: seeded violations plus the records that must stay silent.
/// The expectation is sorted into the lane's canonical `(record_id, rule)`
/// byte-wise order (`docs/cli/trust-audit.md` §Determinism) before it is
/// zipped against the output below.
fn violation_fixture() -> (Vec<GraphRecord>, Vec<ExpectedViolation>) {
    let (mut records, mut expected) = seeded_violations();
    records.extend(clean_fixture_records());
    expected.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.3.cmp(&b.3)));
    (records, expected)
}

/// A fixture with zero violations: consistent producer classes plus one
/// legacy record.
fn clean_fixture() -> Vec<GraphRecord> {
    vec![
        stamped(
            codegraph(GraphRecord::node(
                "trust-audit:clean:sym".to_owned(),
                NodeKind::Symbol,
                Some("src/lib.rs".to_owned()),
                None,
                Some("clean_symbol".to_owned()),
                "fixture symbol".to_owned(),
            )),
            ProducerKind::CodeGraphExtractor,
        ),
        stamped(
            agent_memory(GraphRecord::node(
                "trust-audit:clean:obs".to_owned(),
                NodeKind::Observation,
                None,
                None,
                None,
                "fixture observation".to_owned(),
            )),
            ProducerKind::TaskWriter,
        ),
        stamped(
            GraphRecord::edge(
                EdgeLabel::Contains,
                "trust-audit:clean:sym".to_owned(),
                "trust-audit:clean:obs".to_owned(),
                None,
                "fixture edge".to_owned(),
            ),
            // The edge is audited against its *source* node's kind: the
            // source is a code-fact `Symbol`, so the edge needs a
            // deterministic-class producer to stay clean.
            ProducerKind::CodeGraphExtractor,
        ),
        codegraph(GraphRecord::node(
            "trust-audit:clean:legacy".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            None,
            "fixture symbol".to_owned(),
        )),
    ]
}

fn write_graph(records: &[GraphRecord]) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let mut jsonl = String::new();
    for record in records {
        jsonl.push_str(&serde_json::to_string(record).expect("record serializes"));
        jsonl.push('\n');
    }
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, &jsonl).expect("write graph");
    (temp, graph)
}

fn run_trust_audit(graph: &std::path::Path, format: &str) -> std::process::Output {
    egregore()
        .args(["query", "trust-audit", "--graph"])
        .arg(graph)
        .args(["--format", format])
        .output()
        .expect("trust-audit should run")
}

fn assert_success(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "trust-audit failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).expect("stdout should be one JSON document")
}

#[test]
fn trust_audit_flags_exactly_the_seeded_violations() {
    let (records, expected) = violation_fixture();
    let (_temp, graph) = write_graph(&records);
    let value = assert_success(&run_trust_audit(&graph, "json"));

    assert_eq!(value["ok"], Value::Bool(true), "ok should be true");
    let violations = value["violations"].as_array().expect("violations array");
    assert_eq!(
        violations.len(),
        expected.len(),
        "exactly the seeded violations, no more, no fewer: {violations:?}"
    );

    for (violation, (record_id, kind, producer_kind, rule)) in
        violations.iter().zip(expected.iter())
    {
        assert_eq!(violation["record_id"].as_str(), Some(record_id.as_str()));
        assert_eq!(violation["kind"].as_str(), Some(kind.as_str()));
        assert_eq!(
            violation["producer_kind"].as_str(),
            Some(producer_kind.as_str())
        );
        assert_eq!(violation["rule"].as_str(), Some(rule.as_str()));
    }

    // The headline violation (agent producer under a code-fact kind) carries
    // the citable handle and span — never synthesized prose.
    let headline = violations
        .iter()
        .find(|v| v["record_id"] == "trust-audit:sym:agent-produced")
        .expect("headline violation present");
    assert_eq!(headline["repo_relative_path"], "src/lib.rs");
    let span = headline["span"].as_object().expect("span object");
    assert_eq!(span["start_line"], Value::from(3));

    // The counts are an affirmative total, not silence.
    let counts = value["counts"].as_object().expect("counts object");
    assert_eq!(counts["records_scanned"], Value::from(records.len() as u64));
    assert_eq!(counts["violations"], Value::from(expected.len() as u64));
}

#[test]
fn trust_audit_clean_store_is_explicit_ok() {
    let records = clean_fixture();
    let (_temp, graph) = write_graph(&records);
    let value = assert_success(&run_trust_audit(&graph, "json"));

    assert_eq!(value["ok"], Value::Bool(true));
    assert_eq!(
        value["violations"],
        Value::Array(Vec::new()),
        "clean store: explicit empty violations, not silence"
    );
    let counts = value["counts"].as_object().expect("counts object");
    assert_eq!(counts["records_scanned"], Value::from(records.len() as u64));
    assert_eq!(counts["violations"], Value::from(0));
}

#[test]
fn trust_audit_is_byte_identical_across_runs() {
    let (records, _) = violation_fixture();
    let (_temp, graph) = write_graph(&records);

    let first = run_trust_audit(&graph, "json");
    assert!(first.status.success());
    for _ in 0..2 {
        let again = run_trust_audit(&graph, "json");
        assert!(again.status.success());
        assert_eq!(
            first.stdout, again.stdout,
            "trust-audit output must be byte-identical across runs"
        );
    }
}

#[test]
fn trust_audit_is_read_only() {
    let (records, _) = violation_fixture();
    let (_temp, graph) = write_graph(&records);
    let before = fs::read(&graph).expect("read graph before");

    let value = assert_success(&run_trust_audit(&graph, "json"));

    let after = fs::read(&graph).expect("read graph after");
    assert_eq!(before, after, "trust-audit must not modify the input store");
    // The scanned total still matches the file's record count: zero records
    // created or deleted.
    assert_eq!(
        value["counts"]["records_scanned"],
        Value::from(records.len() as u64)
    );
}

#[test]
fn trust_audit_text_format_names_each_violation() {
    let (records, expected) = violation_fixture();
    let (_temp, graph) = write_graph(&records);
    let output = run_trust_audit(&graph, "text");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for (record_id, _, _, _) in &expected {
        assert!(
            stdout.contains(record_id.as_str()),
            "text output should name {record_id}"
        );
    }
}
