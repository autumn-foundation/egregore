//! End-to-end tests for `eg query session` — the one-session retrospective
//! (issue #259). The lane pivots on a single `AgentSession` and returns a
//! trust-separated retrospective: deterministic code-graph footprint
//! (`touched`), agent-authored claims (`observations`, `failures`), produced
//! artifacts (`artifacts`), and verification-domain evidence
//! (`verification_evidence`), plus a `verification_status` derived ONLY from
//! verification-domain evidence — never from agent claims.
//!
//! RED-FIRST: this file is written before the lane exists; every test here
//! fails (unrecognized subcommand) until the lane ships.

#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    session_id: String,
    source_handle: String,
    file_path: String,
    symbol_id: String,
    observation_id: String,
    failure_id: String,
    patch_id: String,
    verification_id: String,
}

/// Seeds a session graph with every record class named in AC8: one
/// `AgentSession` (with run + turn members), a `TOUCHED_FILE`/`MENTIONS_SYMBOL`
/// footprint, an agent `Observation`, a `Failure` claim with a `FAILED_ON`
/// edge, a `PatchArtifact` via `PRODUCED_PATCH`, and a passing `Verification`
/// stamped with the session id.
#[allow(clippy::too_many_lines)]
fn seed(verification_status: Option<&str>) -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("session_retro.jsonl");
    let mut records: Vec<GraphRecord> = Vec::new();

    // ── AgentSession (the pivot) ──────────────────────────────────────────
    let session_id = agent_memory_stable_id(&["node", "agent_session", "sess_259"]);
    let source_handle = "sess-259.traj:blake3:deadbeef".to_owned();
    let mut session = GraphRecord::node(
        session_id.clone(),
        NodeKind::AgentSession,
        None,
        None,
        Some("AgentSession sess_259".to_owned()),
        "AgentSession sess_259 trajectory".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        source_handle: sh,
        agent_kind,
        ..
    } = &mut session
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some(session_id.clone());
        *sh = Some(source_handle.clone());
        *agent_kind = Some("test-harness".to_owned());
    }
    records.push(session);

    // ── AgentRun -SESSION_OF-> AgentSession ───────────────────────────────
    let run_id = agent_memory_stable_id(&["node", "agent_run", "run_259"]);
    let mut run = GraphRecord::node(
        run_id.clone(),
        NodeKind::AgentRun,
        None,
        None,
        None,
        "AgentRun outcome=done".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        ..
    } = &mut run
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some(session_id.clone());
    }
    records.push(run);
    records.push(GraphRecord::edge(
        EdgeLabel::SessionOf,
        run_id.clone(),
        session_id.clone(),
        None,
        "run belongs to session".to_owned(),
    ));

    // ── AgentTurn -AUTHORED_BY-> AgentRun ──────────────────────────────────
    let turn_id = agent_memory_stable_id(&["node", "agent_turn", "turn_259"]);
    let mut turn = GraphRecord::node(
        turn_id.clone(),
        NodeKind::AgentTurn,
        None,
        None,
        None,
        "AgentTurn 0".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        ..
    } = &mut turn
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some(session_id.clone());
    }
    records.push(turn);
    records.push(GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        turn_id.clone(),
        run_id,
        None,
        "turn authored by run".to_owned(),
    ));

    // Code-graph records use codegraph-domain IDs (matching `syntax_node`'s
    // codegraph schema version); agent-memory IDs would fail schema-version
    // validation on load.
    let file_path = "src/lib.rs".to_owned();
    let file_id = stable_id(&["node", "File", &file_path]);
    records.push(GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        file_path.clone(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    ));

    let symbol_id = stable_id(&["node", "Symbol", &file_path, "foo"]);
    records.push(GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        file_path.clone(),
        span(10, 20),
        "foo".to_owned(),
        "rust",
        "Symbol foo".to_owned(),
    ));

    // ── Deterministic footprint: turn -TOUCHED_FILE-> file,
    //    turn -MENTIONS_SYMBOL-> symbol ────────────────────────────────────
    records.push(GraphRecord::edge(
        EdgeLabel::TouchedFile,
        turn_id.clone(),
        file_id,
        None,
        "session touched lib.rs".to_owned(),
    ));
    records.push(GraphRecord::edge(
        EdgeLabel::MentionsSymbol,
        turn_id.clone(),
        symbol_id.clone(),
        None,
        "session mentioned foo".to_owned(),
    ));

    // ── Agent-authored observation ────────────────────────────────────────
    let observation_id = agent_memory_stable_id(&["node", "observation", "obs_259"]);
    let mut observation = GraphRecord::node(
        observation_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation: foo looks correct".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        text,
        ..
    } = &mut observation
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some(session_id.clone());
        *text = Some("foo looks correct".to_owned());
    }
    records.push(observation);
    records.push(GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        observation_id.clone(),
        turn_id.clone(),
        None,
        "observation authored by turn".to_owned(),
    ));

    // ── Agent failure claim + FAILED_ON edge ──────────────────────────────
    let failure_id = agent_memory_stable_id(&["node", "failure", "fail_259"]);
    let mut failure = GraphRecord::node(
        failure_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "Failure: tests broke on foo".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        failure_kind,
        ..
    } = &mut failure
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some(session_id.clone());
        *failure_kind = Some("test_failure".to_owned());
    }
    records.push(failure);
    records.push(GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        failure_id.clone(),
        turn_id.clone(),
        None,
        "failure authored by turn".to_owned(),
    ));
    records.push(GraphRecord::edge(
        EdgeLabel::FailedOn,
        failure_id.clone(),
        symbol_id.clone(),
        None,
        "failure describes foo".to_owned(),
    ));

    // ── Produced patch artifact ───────────────────────────────────────────
    let patch_id = agent_memory_stable_id(&["node", "patch", "patch_259"]);
    let mut patch_record = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "Patch artifact fixing foo".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        patch_status,
        target_files,
        ..
    } = &mut patch_record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *patch_status = Some("validated".to_owned());
        *target_files = Some(vec![file_path.clone()]);
    }
    records.push(patch_record);
    records.push(GraphRecord::edge(
        EdgeLabel::ProducedPatch,
        turn_id,
        patch_id.clone(),
        None,
        "turn produced patch".to_owned(),
    ));

    // ── Passing verification, stamped with the session id ─────────────────
    // Verification records carry the `verification:v{N}:` ID prefix; the lane
    // requires kind *and* ID to classify verification evidence.
    let verification_id = verification_stable_id(&["node", "verification", "ver_259"]);
    if let Some(status) = verification_status {
        let mut verification = GraphRecord::node(
            verification_id.clone(),
            NodeKind::Verification,
            None,
            None,
            None,
            "cargo test passed".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            session_id: sid,
            status: st,
            verification_kind,
            executed_at,
            ..
        } = &mut verification
        {
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *sid = Some(session_id.clone());
            *st = Some(status.to_owned());
            *verification_kind = Some("test_run".to_owned());
            *executed_at = Some("2026-09-15T09:00:00Z".to_owned());
        }
        records.push(verification);
    }

    let mut graph = Graph::new();
    for r in records {
        graph.push(r);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");
    Fixture {
        _temp: temp,
        graph: path,
        session_id,
        source_handle,
        file_path,
        symbol_id,
        observation_id,
        failure_id,
        patch_id,
        verification_id,
    }
}

/// Runs `eg query session <id-or-handle> --graph <fixture> [extra...]`.
fn run(fx: &Fixture, id_or_handle: &str, extra: &[&str]) -> (i32, String, String) {
    let assert = egregore()
        .args(["query", "session", id_or_handle, "--graph"])
        .arg(&fx.graph)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

/// Every row of every returned section must carry a `record_id` and at least
/// one citation handle.
fn assert_items_cited(v: &serde_json::Value, section: &str) {
    let items = v[section].as_array().expect("section is an array");
    for item in items {
        assert!(
            item["record_id"].as_str().is_some_and(|s| !s.is_empty()),
            "row in {section} must carry a record_id: {item}"
        );
        assert!(
            item["citations"].as_array().is_some_and(|c| !c.is_empty()),
            "row in {section} must carry >=1 citation handle: {item}"
        );
    }
}

#[test]
fn session_lane_returns_trust_separated_sections() {
    let fx = seed(Some("pass"));
    let (code, stdout, stderr) = run(&fx, &fx.session_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(v["ok"], true);
    assert_eq!(v["session_id"], fx.session_id);

    // ── touched: deterministic footprint only ─────────────────────────────
    let touched = v["touched"].as_array().expect("touched array");
    assert_eq!(touched.len(), 2, "file + symbol footprint: {stdout}");
    for row in touched {
        assert_eq!(
            row["trust"], "source_derived",
            "no agent claim may appear in touched: {row}"
        );
        assert!(
            ["File", "Symbol"].contains(&row["kind"].as_str().unwrap_or("")),
            "touched rows are code-graph facts only: {row}"
        );
    }
    assert!(
        touched
            .iter()
            .all(|r| r["record_id"].as_str() != Some(fx.file_path.as_str())),
        "touched rows carry record ids, not paths: {stdout}"
    );
    assert!(
        touched
            .iter()
            .any(|r| r["repo_relative_path"] == fx.file_path)
    );
    assert!(
        touched.iter().any(|r| r["record_id"] == fx.symbol_id),
        "the touched symbol row must carry its record id: {stdout}"
    );

    // ── observations: agent-authored ──────────────────────────────────────
    let observations = v["observations"].as_array().expect("observations array");
    assert_eq!(observations.len(), 1, "one observation: {stdout}");
    assert_eq!(observations[0]["record_id"], fx.observation_id);
    assert!(
        observations[0]["trust"]
            .as_str()
            .unwrap_or("")
            .starts_with("agent_"),
        "observations are agent-authored: {}",
        observations[0]
    );

    // ── failures: agent Failure claim, citing the failed-on symbol ────────
    let failures = v["failures"].as_array().expect("failures array");
    assert_eq!(failures.len(), 1, "one failure: {stdout}");
    assert_eq!(failures[0]["record_id"], fx.failure_id);
    let failure_citations = failures[0]["citations"].as_array().expect("citations");
    assert!(
        failure_citations
            .iter()
            .any(|c| c.as_str().is_some_and(|s| s.contains("src/lib.rs"))),
        "failure must cite its failed-on code handle: {failures:?}"
    );

    // ── artifacts: the produced patch ─────────────────────────────────────
    let artifacts = v["artifacts"].as_array().expect("artifacts array");
    assert_eq!(artifacts.len(), 1, "one artifact: {stdout}");
    assert_eq!(artifacts[0]["record_id"], fx.patch_id);
    assert_eq!(artifacts[0]["trust"], "artifact");

    // ── verification_evidence: the passing check ──────────────────────────
    let ver = v["verification_evidence"]
        .as_array()
        .expect("verification_evidence array");
    assert_eq!(ver.len(), 1, "one verification record: {stdout}");
    assert_eq!(ver[0]["record_id"], fx.verification_id);
    assert_eq!(ver[0]["trust"], "verification_evidence");

    // ── verification_status derived ONLY from verification evidence ───────
    assert_eq!(v["verification_status"], "verified");

    // ── citation completeness on every section ────────────────────────────
    for section in [
        "touched",
        "observations",
        "failures",
        "artifacts",
        "verification_evidence",
    ] {
        assert_items_cited(&v, section);
    }

    // ── standing disclaimer ───────────────────────────────────────────────
    assert!(
        v["disclaimer"]
            .as_str()
            .is_some_and(|d| d.contains("No footprint found is not evidence the run did nothing")),
        "standing disclaimer required: {stdout}"
    );
}

#[test]
fn session_resolves_by_imported_source_handle() {
    let fx = seed(Some("pass"));
    let (code, stdout, stderr) = run(&fx, &fx.source_handle, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["session_id"], fx.session_id);
}

#[test]
fn unknown_session_emits_no_match_exit_2() {
    let fx = seed(Some("pass"));
    let (code, stdout, _stderr) = run(&fx, "agent_memory:v1:node:agent_session:nope", &[]);
    assert_eq!(code, 2, "unknown session must exit 2, got {code}: {stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match");
}

#[test]
fn ambiguous_source_handle_exits_1() {
    // Two sessions imported from the same artifact path: the handle no longer
    // names exactly one session.
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ambiguous.jsonl");
    let mut graph = Graph::new();
    for (n, sess) in ["sess_a", "sess_b"].iter().enumerate() {
        let id = agent_memory_stable_id(&["node", "agent_session", sess]);
        let mut s = GraphRecord::node(
            id.clone(),
            NodeKind::AgentSession,
            None,
            None,
            Some(format!("AgentSession {sess}")),
            format!("AgentSession {sess}"),
        );
        if let GraphRecord::Node {
            schema_version,
            source_handle,
            session_id: sid,
            ..
        } = &mut s
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            // The imported session handle is the node's `source_handle`
            // (`<artifact path>:<artifact hash>`); two sessions imported from
            // the same artifact *name* only collide when they carry the same
            // handle.
            *source_handle = Some("same.traj".to_owned());
            *sid = Some(format!("sess_{n}"));
        }
        graph.push(s);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let assert = egregore()
        .args(["query", "session", "same.traj", "--graph"])
        .arg(&path)
        .assert();
    let output = assert.get_output().clone();
    assert_eq!(
        output.status.code().unwrap_or(-1),
        1,
        "ambiguous handle must exit 1"
    );
}

#[test]
fn verification_status_unverified_without_checks() {
    let fx = seed(None);
    let (code, stdout, stderr) = run(&fx, &fx.session_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["verification_status"], "unverified");
    assert!(
        v["verification_evidence"].as_array().unwrap().is_empty(),
        "no verification records linked: {stdout}"
    );
}

#[test]
fn verification_status_failed_when_a_linked_check_fails() {
    let fx = seed(Some("fail"));
    let (code, stdout, stderr) = run(&fx, &fx.session_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["verification_status"], "failed");
}

#[test]
fn verification_status_is_not_derived_from_agent_claims() {
    // A passing Verification is ABSENT here; only an Observation *claiming*
    // success exists. The status must stay unverified — never promoted by the
    // agent's own words.
    let fx = seed(None);
    let (code, stdout, stderr) = run(&fx, &fx.session_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["verification_status"], "unverified");
    // The observation claiming "foo looks correct" must not leak into touched.
    let touched = v["touched"].as_array().expect("touched array");
    assert!(
        touched.iter().all(|r| r["trust"] == "source_derived"),
        "touched must stay deterministic: {stdout}"
    );
}

#[test]
fn text_format_renders_sections() {
    let fx = seed(Some("pass"));
    let (code, stdout, stderr) = run(&fx, &fx.session_id, &["--format", "text"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    for marker in [
        "touched",
        "observations",
        "failures",
        "artifacts",
        "verification_evidence",
        "verified",
    ] {
        assert!(
            stdout.contains(marker),
            "text output should name section/status {marker}: {stdout}"
        );
    }
}

#[test]
fn help_text_states_footprint_disclaimer() {
    let assert = egregore().args(["query", "session", "--help"]).assert();
    let output = assert.get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        stdout.contains("No footprint found is not evidence the run did nothing"),
        "help must carry the standing disclaimer: {stdout}"
    );
}
