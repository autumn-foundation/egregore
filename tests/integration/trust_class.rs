//! Derived per-row `trust` labels on cross-domain context answers (issue #114).
//!
//! Every record a cross-domain context answer returns must carry a single
//! `trust` field drawn from a closed vocabulary, so an agent can weight a
//! verified fact above an unverified hypothesis without re-traversing the graph.
//!
//! The failure these tests exist to prevent is the PRD's "cannot distinguish
//! source truth from agent guesses": an agent-authored row must NEVER be
//! labelled `source_derived` or `verification_evidence`.
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
        agent_memory_stable_id, project_stable_id, stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 0,
        start_line,
        end_line,
    }
}

/// Attaches an evidence link to a node record.
fn link(record: &mut GraphRecord, relation: &str, target: &str, target_domain: &str) {
    let GraphRecord::Node { evidence_links, .. } = record else {
        panic!("evidence links require a node record");
    };
    evidence_links
        .get_or_insert_with(Vec::new)
        .push(EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: target_domain.to_owned(),
            relation: relation.to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        });
}

fn observation(seed: &str, summary: &str) -> GraphRecord {
    let mut obs = GraphRecord::node(
        agent_memory_stable_id(&["obs", seed]),
        NodeKind::Observation,
        None,
        None,
        None,
        summary.to_owned(),
    );
    if let GraphRecord::Node {
        agent_id,
        session_id,
        observed_at,
        confidence,
        schema_version,
        ..
    } = &mut obs
    {
        *agent_id = Some("agent:trust_test".to_owned());
        *session_id = Some("session:trust_test".to_owned());
        *observed_at = Some("2026-03-01T12:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    obs
}

fn test_run(seed: &str, run_status: &str) -> GraphRecord {
    let mut run = GraphRecord::node(
        verification_stable_id(&["verification", seed]),
        NodeKind::TestRun,
        None,
        None,
        None,
        format!("TestRun {seed}"),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        ..
    } = &mut run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some(run_status.to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-03-01T12:30:00Z".to_owned());
    }
    run
}

/// IDs of the records seeded by [`fixture_trust_graph`].
struct TrustFixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    symbol_id: String,
    file_id: String,
    unverified_obs_id: String,
    verified_obs_id: String,
    contradicted_obs_id: String,
    fail_backed_obs_id: String,
    passing_run_id: String,
    failing_run_id: String,
    task_id: String,
}

/// The AC5 fixture: a source symbol, an unverified observation, a verified
/// observation, a contradicted observation, an observation backed only by a
/// FAILING verification record, plus verification and project records — every
/// trust class in one answer.
#[allow(clippy::too_many_lines)]
fn fixture_trust_graph() -> TrustFixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("trust.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let symbol_id = stable_id(&["node", "Symbol", "src/lib.rs", "trusted_function"]);

    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 80),
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );
    let symbol = GraphRecord::symbol(
        symbol_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "trusted_function".to_owned(),
        "Rust fn trusted_function".to_owned(),
    );
    let defines = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        symbol_id.clone(),
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let passing_run = test_run("trust_pass", "passed");
    let passing_run_id = passing_run.id().to_owned();
    let failing_run = test_run("trust_fail", "failed");
    let failing_run_id = failing_run.id().to_owned();

    // (a) unverified: mentions the symbol, cites no verification record.
    let mut unverified = observation(
        "trust_unverified",
        "unverified claim about trusted_function",
    );
    link(&mut unverified, "MENTIONS_SYMBOL", &symbol_id, "codegraph");
    let unverified_obs_id = unverified.id().to_owned();

    // (b) verified: mentions the symbol AND cites the PASSING TestRun.
    let mut verified = observation("trust_verified", "verified claim about trusted_function");
    link(&mut verified, "MENTIONS_SYMBOL", &symbol_id, "codegraph");
    link(
        &mut verified,
        "VALIDATED_BY",
        &passing_run_id,
        "verification",
    );
    let verified_obs_id = verified.id().to_owned();

    // (c) backed only by a FAILING TestRun — must NOT resolve to agent_verified.
    let mut fail_backed = observation("trust_fail_backed", "claim backed by a failing run");
    link(&mut fail_backed, "MENTIONS_SYMBOL", &symbol_id, "codegraph");
    link(
        &mut fail_backed,
        "VALIDATED_BY",
        &failing_run_id,
        "verification",
    );
    let fail_backed_obs_id = fail_backed.id().to_owned();

    // (d) contradicted: mentions the symbol, cites the passing run (so
    // contradiction must WIN over verification), and is contradicted by a later
    // observation via a CONTRADICTS edge.
    let mut contradicted = observation("trust_contradicted", "contradicted claim");
    link(
        &mut contradicted,
        "MENTIONS_SYMBOL",
        &symbol_id,
        "codegraph",
    );
    link(
        &mut contradicted,
        "VALIDATED_BY",
        &passing_run_id,
        "verification",
    );
    let contradicted_obs_id = contradicted.id().to_owned();

    let rebuttal = observation("trust_rebuttal", "rebuttal claim");
    let rebuttal_id = rebuttal.id().to_owned();
    let contradicts_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::Contradicts,
        rebuttal_id,
        contradicted_obs_id.clone(),
        Some("1.0".to_owned()),
        "rebuttal contradicts claim".to_owned(),
    );

    // Verification record linked to the symbol so it lands in the answer's
    // `verification_evidence` section.
    let mut symbol_verification = test_run("trust_symbol_ver", "passed");
    link(
        &mut symbol_verification,
        "VALIDATED_BY",
        &symbol_id,
        "codegraph",
    );

    // Project record linked to the symbol.
    let mut task = GraphRecord::node(
        project_stable_id(&["task", "trust_task"]),
        NodeKind::Task,
        None,
        None,
        Some("Harden trusted_function".to_owned()),
        "Task: harden trusted_function".to_owned(),
    );
    if let GraphRecord::Node {
        title,
        schema_version,
        ..
    } = &mut task
    {
        *title = Some("Harden trusted_function".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }
    link(&mut task, "MENTIONS_SYMBOL", &symbol_id, "codegraph");
    let task_id = task.id().to_owned();

    let mut graph = Graph::new();
    graph.push(file);
    graph.push(symbol);
    graph.push(defines);
    graph.push(passing_run);
    graph.push(failing_run);
    graph.push(unverified);
    graph.push(verified);
    graph.push(fail_backed);
    graph.push(contradicted);
    graph.push(rebuttal);
    graph.push(contradicts_edge);
    graph.push(symbol_verification);
    graph.push(task);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write fixture");

    TrustFixture {
        _temp: temp,
        path,
        symbol_id,
        file_id,
        unverified_obs_id,
        verified_obs_id,
        contradicted_obs_id,
        fail_backed_obs_id,
        passing_run_id,
        failing_run_id,
        task_id,
    }
}

/// Runs `eg query context trusted_function` over the fixture and returns the
/// parsed envelope.
fn run_context(fixture: &TrustFixture, extra: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("eg").expect("binary");
    cmd.arg("query")
        .arg("context")
        .arg("trusted_function")
        .arg("--graph")
        .arg(&fixture.path);
    for arg in extra {
        cmd.arg(arg);
    }
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).expect("context envelope is JSON")
}

/// Every array-valued section of a context envelope that carries records.
const RECORD_SECTIONS: &[&str] = &[
    "source_facts",
    "topology_edges",
    "observations",
    "project_state",
    "artifacts",
    "verification_evidence",
    "drift_history",
];

fn rows<'a>(envelope: &'a Value, section: &str) -> Vec<&'a Value> {
    envelope
        .get(section)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn trust_of(envelope: &Value, record_id: &str) -> String {
    for section in RECORD_SECTIONS {
        for row in rows(envelope, section) {
            if row.get("record_id").and_then(Value::as_str) == Some(record_id) {
                return row
                    .get("trust")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("row {record_id} in {section} carries no `trust`"))
                    .to_owned();
            }
        }
    }
    panic!("record {record_id} not present in any context section");
}

/// The closed `trust` vocabulary (AC1).
const TRUST_VOCABULARY: &[&str] = &[
    "source_derived",
    "verification_evidence",
    "agent_verified",
    "agent_unverified",
    "agent_contradicted",
    "project_state",
    "artifact",
    "runtime_observation",
    "other",
];

// ---------------------------------------------------------------------------
// AC1 — every returned record carries a `trust` field from a closed vocabulary
// ---------------------------------------------------------------------------

#[test]
fn ac1_every_context_record_carries_a_trust_field_from_the_closed_vocabulary() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &[]);

    let mut seen = 0_usize;
    for section in RECORD_SECTIONS {
        for row in rows(&envelope, section) {
            let trust = row.get("trust").and_then(Value::as_str).unwrap_or_else(|| {
                panic!("row in `{section}` carries no `trust` field: {row}");
            });
            assert!(
                TRUST_VOCABULARY.contains(&trust),
                "row in `{section}` carries `trust` outside the closed vocabulary: {trust}"
            );
            seen += 1;
        }
    }
    assert!(seen > 0, "fixture returned no rows to label");
}

// ---------------------------------------------------------------------------
// AC2 — code-graph / semantic records are source_derived; verification records
// are verification_evidence; neither is ever agent-labelled
// ---------------------------------------------------------------------------

#[test]
fn ac2_code_graph_records_are_source_derived_and_verification_is_verification_evidence() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &[]);

    assert_eq!(trust_of(&envelope, &fixture.symbol_id), "source_derived");
    assert_eq!(trust_of(&envelope, &fixture.file_id), "source_derived");

    for row in rows(&envelope, "topology_edges") {
        assert_eq!(
            row.get("trust").and_then(Value::as_str),
            Some("source_derived"),
            "topology edge is not source_derived: {row}"
        );
    }

    let verification_rows = rows(&envelope, "verification_evidence");
    assert!(
        !verification_rows.is_empty(),
        "fixture returned no verification evidence"
    );
    for row in verification_rows {
        assert_eq!(
            row.get("trust").and_then(Value::as_str),
            Some("verification_evidence"),
            "verification row is not verification_evidence: {row}"
        );
    }

    assert_eq!(trust_of(&envelope, &fixture.task_id), "project_state");
}

// ---------------------------------------------------------------------------
// AC3 — the three agent-authored classes
// ---------------------------------------------------------------------------

#[test]
fn ac3_unverified_observation_resolves_to_agent_unverified() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &[]);
    assert_eq!(
        trust_of(&envelope, &fixture.unverified_obs_id),
        "agent_unverified"
    );
}

#[test]
fn ac3_observation_linked_to_passing_verification_resolves_to_agent_verified() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &[]);
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
}

#[test]
fn ac3_observation_backed_only_by_a_failing_run_is_not_agent_verified() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &[]);
    assert_eq!(
        trust_of(&envelope, &fixture.fail_backed_obs_id),
        "agent_unverified",
        "a FAILING verification record must never confer agent_verified"
    );
}

#[test]
fn ac3_contradicted_observation_resolves_to_agent_contradicted() {
    let fixture = fixture_trust_graph();
    // `--supersession include-but-flag` keeps the displaced row in the answer so
    // its label is observable alongside `temporal_status`.
    let envelope = run_context(&fixture, &["--supersession", "include-but-flag"]);
    assert_eq!(
        trust_of(&envelope, &fixture.contradicted_obs_id),
        "agent_contradicted",
        "contradiction must dominate the passing verification link this row also carries"
    );

    // The derived label can never disagree with the `temporal_status` field that
    // the same resolver drives.
    let row = rows(&envelope, "observations")
        .into_iter()
        .find(|r| r.get("record_id").and_then(Value::as_str) == Some(&fixture.contradicted_obs_id))
        .expect("contradicted observation is present under include-but-flag");
    assert_eq!(
        row.get("temporal_status").and_then(Value::as_str),
        Some("contradicted")
    );
}

#[test]
fn ac3_excluded_contradicted_row_still_carries_its_trust_label() {
    let fixture = fixture_trust_graph();
    // Default supersession mode drops the contradicted row into `excluded`.
    let envelope = run_context(&fixture, &[]);
    let excluded = rows(&envelope, "excluded");
    let row = excluded
        .into_iter()
        .find(|r| r.get("record_id").and_then(Value::as_str) == Some(&fixture.contradicted_obs_id))
        .expect("contradicted observation is excluded by default");
    assert_eq!(
        row.get("trust").and_then(Value::as_str),
        Some("agent_contradicted")
    );
}

// ---------------------------------------------------------------------------
// AC4 — deterministic: byte-identical labels across runs
// ---------------------------------------------------------------------------

#[test]
fn ac4_trust_labels_are_byte_identical_across_runs() {
    let fixture = fixture_trust_graph();
    let mut renders = Vec::new();
    for _ in 0..3 {
        let mut cmd = Command::cargo_bin("eg").expect("binary");
        let out = cmd
            .arg("query")
            .arg("context")
            .arg("trusted_function")
            .arg("--graph")
            .arg(&fixture.path)
            .arg("--supersession")
            .arg("include-but-flag")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        renders.push(out);
    }
    assert_eq!(renders[0], renders[1], "context answer is not byte-stable");
    assert_eq!(renders[1], renders[2], "context answer is not byte-stable");
}

// ---------------------------------------------------------------------------
// AC5 — the mixed fixture: zero agent rows mislabelled as source truth
// ---------------------------------------------------------------------------

#[test]
fn ac5_mixed_fixture_labels_every_class_distinctly_with_zero_mislabels() {
    let fixture = fixture_trust_graph();
    let envelope = run_context(&fixture, &["--supersession", "include-but-flag"]);

    // Distinct labels for the four seeded shapes.
    assert_eq!(trust_of(&envelope, &fixture.symbol_id), "source_derived");
    assert_eq!(
        trust_of(&envelope, &fixture.unverified_obs_id),
        "agent_unverified"
    );
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
    assert_eq!(
        trust_of(&envelope, &fixture.contradicted_obs_id),
        "agent_contradicted"
    );

    // Zero agent-authored rows labelled as source truth or verification.
    for row in rows(&envelope, "observations") {
        let trust = row.get("trust").and_then(Value::as_str).expect("trust");
        assert!(
            trust.starts_with("agent_"),
            "an agent-authored row was labelled `{trust}`: {row}"
        );
    }

    // And no code/verification row leaked an agent class.
    for section in ["source_facts", "topology_edges", "verification_evidence"] {
        for row in rows(&envelope, section) {
            let trust = row.get("trust").and_then(Value::as_str).expect("trust");
            assert!(
                !trust.starts_with("agent_"),
                "a `{section}` row was labelled `{trust}`: {row}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The same guarantee on the other cross-domain context lanes
// ---------------------------------------------------------------------------

#[test]
fn subsystem_lane_labels_every_record_with_a_trust_class() {
    let fixture = fixture_trust_graph();
    let out = Command::cargo_bin("eg")
        .expect("binary")
        .arg("query")
        .arg("subsystem")
        .arg("src")
        .arg("--graph")
        .arg(&fixture.path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&out).expect("subsystem envelope is JSON");

    let mut seen = 0_usize;
    for section in RECORD_SECTIONS {
        for row in rows(&envelope, section) {
            let trust = row.get("trust").and_then(Value::as_str).unwrap_or_else(|| {
                panic!("subsystem row in `{section}` carries no `trust`: {row}")
            });
            assert!(TRUST_VOCABULARY.contains(&trust));
            seen += 1;
        }
    }
    assert!(seen > 0, "subsystem answer returned no rows");
    assert_eq!(trust_of(&envelope, &fixture.symbol_id), "source_derived");
    assert_eq!(
        trust_of(&envelope, &fixture.unverified_obs_id),
        "agent_unverified"
    );
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
}

#[test]
fn locate_lane_labels_every_record_with_a_trust_class() {
    let fixture = fixture_trust_graph();
    let out = Command::cargo_bin("eg")
        .expect("binary")
        .arg("query")
        .arg("locate")
        .arg("src/lib.rs:12")
        .arg("--graph")
        .arg(&fixture.path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&out).expect("locate envelope is JSON");

    assert_eq!(trust_of(&envelope, &fixture.symbol_id), "source_derived");
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
    assert_eq!(
        trust_of(&envelope, &fixture.unverified_obs_id),
        "agent_unverified"
    );

    // The anchor rows are records too, and they are NOT inside any of the array
    // sections `trust_of` scans — a labelled `source_facts` copy of the same
    // symbol must not be mistaken for the anchor carrying its own label.
    assert_eq!(
        envelope["symbol"]["trust"].as_str(),
        Some("source_derived"),
        "the located symbol itself must carry `trust`"
    );
    for row in envelope["enclosing_chain"]
        .as_array()
        .expect("enclosing_chain is an array")
    {
        let trust = row["trust"]
            .as_str()
            .unwrap_or_else(|| panic!("enclosing_chain row carries no `trust`: {row}"));
        assert!(TRUST_VOCABULARY.contains(&trust));
    }
}

#[test]
fn at_lane_labels_the_located_symbol_and_chain() {
    let fixture = fixture_trust_graph();
    let out = Command::cargo_bin("eg")
        .expect("binary")
        .arg("query")
        .arg("at")
        .arg("src/lib.rs:12")
        .arg("--graph")
        .arg(&fixture.path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&out).expect("at envelope is JSON");
    assert_eq!(
        envelope["symbol"]["trust"].as_str(),
        Some("source_derived"),
        "`eg query at` shares the located-node row shape and must label it too"
    );
}

/// An agent must not be able to certify its own claim by writing a
/// `CommandEvidence` record: it is minted in the agent-memory domain from the
/// agent's own `--exit-code`. End-to-end over the real CLI.
#[test]
fn agent_minted_command_evidence_does_not_confer_agent_verified_end_to_end() {
    let fixture = fixture_trust_graph();

    // A verification-SHAPED node with an agent-memory record ID and a passing
    // status, cited VALIDATED_BY from a fresh observation on the same symbol.
    let mut self_signed = GraphRecord::node(
        agent_memory_stable_id(&["node", "command_evidence", "self_signed"]),
        NodeKind::CommandEvidence,
        None,
        None,
        None,
        "self-reported command evidence".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        exit_code,
        ..
    } = &mut self_signed
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *exit_code = Some(0);
    }
    let self_signed_id = self_signed.id().to_owned();

    let mut claim = observation("trust_self_certified", "self-certified claim");
    link(
        &mut claim,
        "MENTIONS_SYMBOL",
        &fixture.symbol_id,
        "codegraph",
    );
    link(&mut claim, "VALIDATED_BY", &self_signed_id, "verification");
    let claim_id = claim.id().to_owned();

    let mut lines: Vec<String> = fs::read_to_string(&fixture.path)
        .expect("read fixture")
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    for record in [&self_signed, &claim] {
        lines.push(serde_json::to_string(record).expect("serialize"));
    }
    let path = fixture.path.with_file_name("trust_self_certified.jsonl");
    fs::write(&path, lines.join("\n")).expect("write");

    let out = Command::cargo_bin("eg")
        .expect("binary")
        .arg("query")
        .arg("context")
        .arg("trusted_function")
        .arg("--graph")
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&out).expect("context envelope is JSON");

    assert_eq!(
        trust_of(&envelope, &claim_id),
        "agent_unverified",
        "an agent-minted CommandEvidence must never certify the agent that wrote it"
    );
    // A genuine verification-domain record still confers agent_verified, so the
    // gate discriminates on domain rather than on kind.
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
}

/// The MCP tools are the primary agentic consumption path for these answers, so
/// an unlabelled row there does the most damage.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn mcp_symbol_context_labels_every_record_with_a_trust_class() {
    let fixture = fixture_trust_graph();
    let text = fs::read_to_string(&fixture.path).expect("read fixture");
    let records: Vec<GraphRecord> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse record"))
        .collect();

    let envelope =
        aletheia_egregore::mcp::tool_symbol_context_from_records(&records, "trusted_function");
    assert_eq!(envelope["ok"], Value::Bool(true));

    let mut seen = 0_usize;
    for section in RECORD_SECTIONS {
        for row in rows(&envelope, section) {
            let trust = row
                .get("trust")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("MCP row in `{section}` carries no `trust`: {row}"));
            assert!(TRUST_VOCABULARY.contains(&trust));
            seen += 1;
        }
    }
    assert!(seen > 0, "MCP symbol_context returned no rows");
    assert_eq!(trust_of(&envelope, &fixture.symbol_id), "source_derived");
    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_verified"
    );
    assert_eq!(
        trust_of(&envelope, &fixture.unverified_obs_id),
        "agent_unverified"
    );
}

// ---------------------------------------------------------------------------
// Liveness: a tombstoned verification record confers nothing
// ---------------------------------------------------------------------------

#[test]
fn tombstoned_verification_record_does_not_confer_agent_verified() {
    let fixture = fixture_trust_graph();
    // Re-write the fixture with a tombstone retracting the passing TestRun.
    let mut lines: Vec<String> = fs::read_to_string(&fixture.path)
        .expect("read fixture")
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    let tombstone = GraphRecord::Tombstone {
        id: format!("{}:tombstone", fixture.passing_run_id),
        schema_version: VERIFICATION_SCHEMA_VERSION,
        deleted_id: fixture.passing_run_id.clone(),
        summary: "retracted".to_owned(),
        producer: None,
    };
    lines.push(serde_json::to_string(&tombstone).expect("serialize tombstone"));
    let tombstoned_path = fixture.path.with_file_name("trust_tombstoned.jsonl");
    fs::write(&tombstoned_path, lines.join("\n")).expect("write");

    let out = Command::cargo_bin("eg")
        .expect("binary")
        .arg("query")
        .arg("context")
        .arg("trusted_function")
        .arg("--graph")
        .arg(&tombstoned_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let envelope: Value = serde_json::from_slice(&out).expect("context envelope is JSON");

    assert_eq!(
        trust_of(&envelope, &fixture.verified_obs_id),
        "agent_unverified",
        "a tombstoned verification record must not confer agent_verified"
    );
    // Sanity: the failing run is untouched and still confers nothing.
    assert_eq!(
        trust_of(&envelope, &fixture.fail_backed_obs_id),
        "agent_unverified"
    );
    assert!(!fixture.failing_run_id.is_empty());
}
