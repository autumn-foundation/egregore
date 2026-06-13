#![allow(missing_docs)]

//! End-to-end tests for `eg query failures` — the prior-failed-attempt query
//! (issue #63). The query starts from a code or task handle and returns prior
//! failed attempts as citable local facts, keeping runtime verification failures
//! separate from agent-authored `Failure` claims, surfacing a later passing
//! verification as a separate superseding item, and never leaking raw payloads.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Graph, OutputHandle,
        PROJECT_SCHEMA_VERSION, PatchHandle, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        artifact_stable_id, project_stable_id, stable_id, verification_stable_id,
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
    }
}

/// Sentinel payloads that must NEVER appear in failure-history output (AC8).
const RAW_FAILURE_TEXT_SENTINEL: &str = "RAW_FAILURE_TEXT_SHOULD_NOT_LEAK";
const RAW_STDOUT_SENTINEL: &str = "RAW_STDOUT_SHOULD_NOT_LEAK";
const RAW_PATCH_SENTINEL: &str = "RAW_PATCH_SHOULD_NOT_LEAK";

fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    file_path: String,
    symbol_id: String,
    task_id: String,
    fail1_id: String,
    fail2_id: String,
    runtime_fail_id: String,
    success_id: String,
    patch_id: String,
}

/// Seeds a graph JSONL fixture with every record class named in AC1: a code
/// symbol + file, a task, two prior agent `Failure` attempts, one verification
/// failure, one patch artifact, and one later successful verification. Failure #1
/// (and the runtime failure) precede the later passing verification on the symbol
/// → `since_resolved`; Failure #2 occurs *after* it → `still_failing`.
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("failure_history_seeded.jsonl");
    let mut graph = Graph::new();

    // ── Repository + code handles (so RepositoryIndex can attribute them) ────
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    let repo = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    );

    let file_path = "src/lib.rs".to_owned();
    let file_id = stable_id(&["node", "File", &file_path]);
    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        file_path.clone(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    );

    let symbol_id = stable_id(&["node", "Symbol", &file_path, "foo"]);
    let symbol = GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        file_path.clone(),
        span(10, 20),
        "foo".to_owned(),
        "rust",
        "Symbol foo".to_owned(),
    );

    let contains = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id,
        file_id.clone(),
        None,
        "repo contains file".to_owned(),
    );
    let defines = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        symbol_id.clone(),
        None,
        "file defines symbol".to_owned(),
    );

    // ── Project handle ───────────────────────────────────────────────────────
    let task_id = project_stable_id(&["task", "task_63"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Expose prior failed attempts".to_owned()),
        "Task #63".to_owned(),
    );
    if let GraphRecord::Node {
        title,
        schema_version,
        ..
    } = &mut task
    {
        *title = Some("Expose prior failed attempts".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    // ── Agent + session provenance ───────────────────────────────────────────
    let agent_id = agent_memory_stable_id(&["node", "agent", "agent_1"]);
    let mut agent = GraphRecord::node(
        agent_id.clone(),
        NodeKind::Agent,
        None,
        None,
        Some("agent_1".to_owned()),
        "Agent agent_1".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id: aid,
        ..
    } = &mut agent
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
    }

    let session_id = agent_memory_stable_id(&["node", "agent_session", "agent_1", "sess_1"]);
    let mut session = GraphRecord::node(
        session_id.clone(),
        NodeKind::AgentSession,
        None,
        None,
        Some("sess_1".to_owned()),
        "Session sess_1".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        session_id: sid,
        ..
    } = &mut session
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some("sess_1".to_owned());
    }

    // ── Patch artifact (protected patch bytes must not leak) ─────────────────
    let patch_id = artifact_stable_id(&["patch", "patch_63"]);
    let mut patch_artifact = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "Patch artifact for foo".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        patch_status,
        patch_bytes_hash,
        patch_handle,
        target_files,
        ..
    } = &mut patch_artifact
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *patch_status = Some("rejected_validation".to_owned());
        *patch_bytes_hash = Some("blake3:patchhash".to_owned());
        *patch_handle = Some(Box::new(PatchHandle {
            path: "protected/patch_63.patch".to_owned(),
            inline: Some(RAW_PATCH_SENTINEL.to_owned()),
        }));
        *target_files = Some(vec![file_path.clone()]);
    }

    // ── Runtime verification failure (protected stdout must not leak) ────────
    let runtime_fail_id = verification_stable_id(&["verification", "runtime_fail_63"]);
    let mut runtime_fail = GraphRecord::node(
        runtime_fail_id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "cargo test failed".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        stdout_handle,
        source_artifact_path,
        evidence_links,
        ..
    } = &mut runtime_fail
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("fail".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-01-15T00:00:00Z".to_owned());
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some(RAW_STDOUT_SENTINEL.to_owned()),
            hash: "blake3:stdouthash".to_owned(),
            bytes: 4096,
        }));
        *source_artifact_path = Some("ci/test.log".to_owned());
        *evidence_links = Some(vec![
            link(&symbol_id, "codegraph", "VALIDATED_BY"),
            link(&file_id, "codegraph", "VALIDATED_BY"),
        ]);
    }

    // ── Later PASSING verification on the same symbol/file ───────────────────
    let success_id = verification_stable_id(&["verification", "success_63"]);
    let mut success = GraphRecord::node(
        success_id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "cargo test passed".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        evidence_links,
        ..
    } = &mut success
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-03-01T00:00:00Z".to_owned());
        *evidence_links = Some(vec![
            link(&symbol_id, "codegraph", "VALIDATED_BY"),
            link(&file_id, "codegraph", "VALIDATED_BY"),
        ]);
    }

    // ── Agent Failure #1 — BEFORE the passing verification (since_resolved) ──
    let fail1_id = agent_memory_stable_id(&["failure", "command_failure", "turn_1", "0"]);
    let missing_target = format!("agent_memory:v1:{}", "0".repeat(64));
    let mut fail1 = GraphRecord::node(
        fail1_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "command failed on foo".to_owned(),
    );
    if let GraphRecord::Node {
        text,
        failure_kind,
        exit_code,
        agent_id: aid,
        agent_kind,
        session_id: sid,
        observed_at,
        source_artifact_path,
        source_artifact_hash,
        schema_version,
        evidence_links,
        ..
    } = &mut fail1
    {
        *text = Some(format!("{RAW_FAILURE_TEXT_SENTINEL}: assertion failed"));
        *failure_kind = Some("command_failure".to_owned());
        *exit_code = Some(1);
        *aid = Some("agent_1".to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *source_artifact_path = Some("trajectories/run-1.traj".to_owned());
        *source_artifact_hash = Some("blake3:trajhash1".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            link(&symbol_id, "codegraph", "FAILED_ON"),
            link(&file_id, "codegraph", "TOUCHED_FILE"),
            link(&task_id, "project", "REFERENCES_TASK"),
            // Unresolved: target absent from the store (AC6).
            link(&missing_target, "agent_memory", "RELATES_TO"),
        ]);
    }

    // ── Agent Failure #2 — AFTER the passing verification (still_failing) ────
    let fail2_id = agent_memory_stable_id(&["failure", "patch_invalid", "turn_2", "0"]);
    let mut fail2 = GraphRecord::node(
        fail2_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "patch invalid for foo".to_owned(),
    );
    if let GraphRecord::Node {
        text,
        failure_kind,
        agent_id: aid,
        session_id: sid,
        observed_at,
        source_artifact_path,
        redaction_policy_version,
        schema_version,
        evidence_links,
        ..
    } = &mut fail2
    {
        *text = Some(format!(
            "{RAW_FAILURE_TEXT_SENTINEL} <REDACTED:api_token:abcd1234ef56>"
        ));
        *failure_kind = Some("patch_invalid".to_owned());
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-04-01T00:00:00Z".to_owned());
        *source_artifact_path = Some("trajectories/run-2.traj".to_owned());
        *redaction_policy_version = Some("1".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            link(&symbol_id, "codegraph", "FAILED_ON"),
            link(&file_id, "codegraph", "TOUCHED_FILE"),
        ]);
    }

    // ── Edges: provenance + produced patch ───────────────────────────────────
    let authored = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        fail1_id.clone(),
        session_id.clone(),
        None,
        "failure authored by session".to_owned(),
    );
    let session_of = GraphRecord::edge(
        EdgeLabel::SessionOf,
        session_id,
        agent_id,
        None,
        "session of agent".to_owned(),
    );
    let produced_patch = GraphRecord::edge(
        EdgeLabel::ProducedPatch,
        fail1_id.clone(),
        patch_id.clone(),
        None,
        "failure produced patch".to_owned(),
    );

    for record in vec![
        repo,
        file,
        symbol,
        contains,
        defines,
        task,
        agent,
        session,
        patch_artifact,
        runtime_fail,
        success,
        fail1,
        fail2,
        authored,
        session_of,
        produced_patch,
    ] {
        graph.push(record);
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    Fixture {
        _temp: temp,
        graph: path,
        file_path,
        symbol_id,
        task_id,
        fail1_id,
        fail2_id,
        runtime_fail_id,
        success_id,
        patch_id,
    }
}

/// Runs `eg query failures <handle> --graph <fixture> [extra...]`.
fn run(fx: &Fixture, handle: &str, extra: &[&str]) -> (i32, String, String) {
    let assert = egregore()
        .args(["query", "failures", handle, "--graph"])
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

#[test]
fn by_symbol_id_returns_both_agent_failures_and_runtime_failure() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(v["ok"], true);
    assert_eq!(v["target_type"], "symbol");

    let agent = v["agent_failures"]
        .as_array()
        .expect("agent_failures array");
    assert_eq!(agent.len(), 2, "two prior agent failures (AC1): {stdout}");
    let runtime = v["runtime_failures"]
        .as_array()
        .expect("runtime_failures array");
    assert_eq!(runtime.len(), 1, "one runtime failure (AC1)");

    let agent_ids: Vec<&str> = agent
        .iter()
        .map(|a| a["record_id"].as_str().unwrap())
        .collect();
    assert!(agent_ids.contains(&fx.fail1_id.as_str()));
    assert!(agent_ids.contains(&fx.fail2_id.as_str()));
    assert_eq!(runtime[0]["record_id"], fx.runtime_fail_id);
}

#[test]
fn separates_runtime_from_agent_trust_classes() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    for a in v["agent_failures"].as_array().unwrap() {
        assert_eq!(a["trust_class"], "agent_authored", "AC4 agent failures");
        assert_eq!(a["kind"], "Failure");
    }
    for r in v["runtime_failures"].as_array().unwrap() {
        assert_eq!(
            r["trust_class"], "verification_evidence",
            "AC4 runtime failures"
        );
    }
}

#[test]
fn each_failure_carries_required_fields() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // AC3: an agent failure carries record ID, failure kind, observed time,
    // agent/session provenance, source artifact path, and a linked target handle.
    let fail1 = v["agent_failures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["record_id"] == fx.fail1_id.as_str())
        .expect("fail1 present");
    assert_eq!(fail1["failure_kind"], "command_failure");
    assert_eq!(fail1["observed_at"], "2026-01-01T00:00:00Z");
    assert_eq!(fail1["agent_id"], "agent_1");
    assert_eq!(fail1["session_id"], "sess_1");
    assert_eq!(fail1["source_artifact_path"], "trajectories/run-1.traj");
    assert_eq!(fail1["matched_target"], fx.symbol_id);
    assert!(
        fail1["citable_handle"]
            .as_str()
            .is_some_and(|h| !h.is_empty())
    );

    // AC3: a runtime failure carries an executed time + verification provenance.
    let runtime = &v["runtime_failures"].as_array().unwrap()[0];
    assert_eq!(runtime["executed_at"], "2026-01-15T00:00:00Z");
    assert_eq!(runtime["status"], "fail");
}

#[test]
fn later_success_is_separate_superseding_item_and_flags_resolution() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // AC5: the later passing verification is a SEPARATE superseding item; the
    // older failures are not hidden.
    let successes = v["superseding_successes"]
        .as_array()
        .expect("superseding_successes array");
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0]["record_id"], fx.success_id);
    assert_eq!(successes[0]["status"], "pass");

    // Issue-comment freshness signal: Failure #1 (before the pass) is
    // since_resolved and cites the pass; Failure #2 (after the pass) still fails.
    let agent = v["agent_failures"].as_array().unwrap();
    let fail1 = agent
        .iter()
        .find(|a| a["record_id"] == fx.fail1_id.as_str())
        .unwrap();
    let fail2 = agent
        .iter()
        .find(|a| a["record_id"] == fx.fail2_id.as_str())
        .unwrap();
    assert_eq!(fail1["resolution_status"], "since_resolved");
    assert_eq!(fail1["resolved_by"], fx.success_id);
    assert_eq!(fail2["resolution_status"], "still_failing");
    assert!(fail2.get("resolved_by").is_none());

    // The runtime failure precedes the pass too → since_resolved.
    assert_eq!(
        v["runtime_failures"].as_array().unwrap()[0]["resolution_status"],
        "since_resolved"
    );
}

#[test]
fn produced_patch_artifact_is_one_hop_and_protected() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let patches = v["patch_artifacts"].as_array().expect("patch_artifacts");
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0]["record_id"], fx.patch_id);
    assert_eq!(patches[0]["protected"], true, "patch bytes withheld (AC8)");
    assert_eq!(patches[0]["patch_bytes_hash"], "blake3:patchhash");
}

#[test]
fn by_file_path_returns_failures() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, &fx.file_path, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["target_type"], "file");
    assert_eq!(v["agent_failures"].as_array().unwrap().len(), 2);
}

#[test]
fn by_task_id_returns_referencing_failure() {
    let fx = seed();
    let (code, stdout, stderr) = run(&fx, &fx.task_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["target_type"], "task");
    let agent = v["agent_failures"].as_array().unwrap();
    assert_eq!(agent.len(), 1, "only failure #1 REFERENCES_TASK");
    assert_eq!(agent[0]["record_id"], fx.fail1_id);
}

#[test]
fn output_has_no_raw_payloads() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(code, 0);
    assert!(
        !stdout.contains(RAW_FAILURE_TEXT_SENTINEL),
        "raw failure text leaked (AC8)"
    );
    assert!(
        !stdout.contains(RAW_STDOUT_SENTINEL),
        "raw command output leaked (AC8)"
    );
    assert!(
        !stdout.contains(RAW_PATCH_SENTINEL),
        "raw patch bytes leaked (AC8)"
    );

    // Protected + redaction diagnostics are surfaced rather than the payloads.
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let codes: Vec<&str> = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"protected_payload"), "codes={codes:?}");
    assert!(codes.contains(&"redacted_payload"), "codes={codes:?}");
    assert!(
        codes.contains(&"unresolved_evidence_link"),
        "AC6 unresolved link surfaced: {codes:?}"
    );
}

#[test]
fn repeated_query_is_deterministic_across_5_runs() {
    let fx = seed();
    let first = run(&fx, &fx.symbol_id, &[]);
    assert_eq!(first.0, 0);
    for _ in 0..4 {
        let again = run(&fx, &fx.symbol_id, &[]);
        assert_eq!(again.1, first.1, "stdout must be byte-identical (AC7)");
        assert_eq!(again.2, first.2, "stderr must be byte-identical (AC7)");
    }
}

#[test]
fn malformed_canonical_handle_exits_1_with_json_error() {
    let fx = seed();
    let (code, _stdout, stderr) = run(&fx, "project:not-a-valid-id", &[]);
    assert_eq!(code, 1, "stderr={stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON error");
    assert!(parsed.get("Unsupported").is_some(), "got {parsed}");
}

#[test]
fn unknown_handle_exits_2_no_match() {
    let fx = seed();
    let (code, stdout, _e) = run(&fx, "does_not_exist", &[]);
    assert_eq!(code, 2);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_match");
}

/// Builds a two-repository store where the symbol name `foo` is ambiguous
/// without `--repo`.
fn seed_two_repos() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("two_repos.jsonl");
    let mut graph = Graph::new();
    for repo in ["repo-a", "repo-b"] {
        let repo_id = stable_id(&["node", "Repository", repo]);
        let file_path = format!("{repo}/src/lib.rs");
        let file_id = stable_id(&["node", "File", &file_path]);
        let symbol_id = stable_id(&["node", "Symbol", &file_path, "foo"]);
        graph.push(GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(repo.to_owned()),
            format!("Repository {repo}"),
        ));
        graph.push(GraphRecord::syntax_node(
            file_id.clone(),
            NodeKind::File,
            file_path.clone(),
            span(1, 100),
            "lib.rs".to_owned(),
            "rust",
            "file".to_owned(),
        ));
        graph.push(GraphRecord::syntax_node(
            symbol_id.clone(),
            NodeKind::Symbol,
            file_path,
            span(1, 10),
            "foo".to_owned(),
            "rust",
            "symbol foo".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            file_id.clone(),
            None,
            "contains".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id,
            symbol_id,
            None,
            "defines".to_owned(),
        ));
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");
    (temp, path)
}

#[test]
fn ambiguous_symbol_across_repos_exits_1_then_resolves_with_repo() {
    let (_temp, graph) = seed_two_repos();
    // Without --repo: ambiguous.
    let out = egregore()
        .args(["query", "failures", "foo", "--graph"])
        .arg(&graph)
        .assert()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON");
    assert!(parsed.get("Ambiguous").is_some(), "got {parsed}");

    // With --repo: resolves (no failures seeded → exit 0, ok:true, empty).
    let out = egregore()
        .args(["query", "failures", "foo", "--graph"])
        .arg(&graph)
        .args(["--repo", "repo-a"])
        .assert()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["agent_failures"].as_array().unwrap().len(), 0);
    assert_eq!(v["runtime_failures"].as_array().unwrap().len(), 0);
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_failures_via_data_dir() {
    let fx = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&fx.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let out = egregore()
        .args(["query", "failures", &fx.symbol_id, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["agent_failures"].as_array().unwrap().len(), 2);
}

// ── Review-fix coverage (issue #63 PR review) ───────────────────────────────

/// Writes records to a fresh JSONL graph and returns the temp dir + path.
fn write_graph(records: Vec<GraphRecord>) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");
    let mut graph = Graph::new();
    for r in records {
        graph.push(r);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");
    (temp, path)
}

/// Runs `eg query failures <handle> --graph <path>`.
fn run_graph(path: &std::path::Path, handle: &str, extra: &[&str]) -> (i32, String, String) {
    let assert = egregore()
        .args(["query", "failures", handle, "--graph"])
        .arg(path)
        .args(extra)
        .assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

fn failure_node(
    id: &str,
    failure_kind: &str,
    observed_at: Option<&str>,
    links: Vec<EvidenceLink>,
) -> GraphRecord {
    let mut n = GraphRecord::node(
        id.to_owned(),
        NodeKind::Failure,
        None,
        None,
        None,
        format!("Failure {failure_kind}"),
    );
    if let GraphRecord::Node {
        failure_kind: fk,
        observed_at: oa,
        schema_version,
        agent_id,
        session_id,
        evidence_links,
        ..
    } = &mut n
    {
        *fk = Some(failure_kind.to_owned());
        *oa = observed_at.map(str::to_owned);
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent_x".to_owned());
        *session_id = Some("sess_x".to_owned());
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    n
}

#[allow(clippy::too_many_arguments)]
fn verification_node(
    id: &str,
    kind: NodeKind,
    status: Option<&str>,
    exit_code: Option<i64>,
    executed_at: Option<&str>,
    observed_at: Option<&str>,
    links: Vec<EvidenceLink>,
) -> GraphRecord {
    let mut n = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        None,
        "verification".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status: s,
        exit_code: ec,
        executed_at: ea,
        observed_at: oa,
        verification_kind,
        evidence_links,
        ..
    } = &mut n
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *s = status.map(str::to_owned);
        *ec = exit_code;
        *ea = executed_at.map(str::to_owned);
        *oa = observed_at.map(str::to_owned);
        *verification_kind = Some("command_run".to_owned());
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    n
}

fn symbol_node(path: &str, name: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Symbol", path, name]);
    let node = GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(1, 10),
        name.to_owned(),
        "rust",
        format!("Symbol {name}"),
    );
    (id, node)
}

#[test]
fn malformed_codegraph_handle_exits_1_unsupported() {
    let fx = seed();
    // A handle with the canonical prefix but a malformed body must be rejected
    // as unsupported (exit 1), distinct from a well-formed but absent ID.
    let (code, _stdout, stderr) = run(&fx, "codegraph:not-a-valid-id", &[]);
    assert_eq!(code, 1, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON error");
    assert!(v.get("Unsupported").is_some(), "got {v}");
}

#[test]
fn tombstoned_task_handle_exits_2_stale() {
    let task_id = project_stable_id(&["task", "deleted_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Deleted task".to_owned()),
        "Task".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut task {
        *schema_version = PROJECT_SCHEMA_VERSION;
    }
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &task_id]),
        schema_version: PROJECT_SCHEMA_VERSION,
        deleted_id: task_id.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };
    let (_t, graph) = write_graph(vec![task, tombstone]);
    let (code, stdout, _e) = run_graph(&graph, &task_id, &[]);
    assert_eq!(code, 2, "stdout={stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["error"]["code"], "stale_handle");
}

#[test]
fn task_query_includes_acceptance_criterion_failures() {
    let task_id = project_stable_id(&["task", "t_ac"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Task with AC".to_owned()),
        "Task".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut task {
        *schema_version = PROJECT_SCHEMA_VERSION;
    }
    let ac_id = project_stable_id(&["ac", "ac_1"]);
    let mut ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC 1".to_owned()),
        "AC".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        parent_task_id,
        ..
    } = &mut ac
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *parent_task_id = Some(task_id.clone());
    }
    // A failure attached to the acceptance criterion, not the task directly.
    let fail = failure_node(
        &agent_memory_stable_id(&["failure", "f_ac"]),
        "command_failure",
        Some("2026-01-01T00:00:00Z"),
        vec![link(&ac_id, "project", "FAILED_ON")],
    );
    let (_t, graph) = write_graph(vec![task, ac, fail]);
    let (code, stdout, stderr) = run_graph(&graph, &task_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(
        v["agent_failures"].as_array().unwrap().len(),
        1,
        "AC-attached failure must surface for a task query"
    );
}

#[test]
fn pass_only_or_pass_before_failure_is_not_superseding() {
    let (symbol_id, symbol) = symbol_node("src/a.rs", "alpha");
    // A passing verification that PREDATES the only failure: it superseded
    // nothing, so it must not appear in superseding_successes (AC5).
    let pass = verification_node(
        &verification_stable_id(&["v", "early_pass"]),
        NodeKind::TestRun,
        Some("pass"),
        None,
        Some("2026-01-01T00:00:00Z"),
        None,
        vec![link(&symbol_id, "codegraph", "VALIDATED_BY")],
    );
    let fail = failure_node(
        &agent_memory_stable_id(&["failure", "later_fail"]),
        "command_failure",
        Some("2026-02-01T00:00:00Z"),
        vec![link(&symbol_id, "codegraph", "FAILED_ON")],
    );
    let (_t, graph) = write_graph(vec![symbol, pass, fail]);
    let (code, stdout, _e) = run_graph(&graph, &symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["agent_failures"].as_array().unwrap().len(), 1);
    assert_eq!(v["agent_failures"][0]["resolution_status"], "still_failing");
    assert_eq!(
        v["superseding_successes"].as_array().unwrap().len(),
        0,
        "a pass before the failure superseded nothing"
    );
}

#[test]
fn failed_on_patch_and_command_run_are_surfaced() {
    let (symbol_id, symbol) = symbol_node("src/b.rs", "beta");
    let fail_id = agent_memory_stable_id(&["failure", "patch_invalid_f"]);
    let fail = failure_node(
        &fail_id,
        "patch_invalid",
        Some("2026-01-01T00:00:00Z"),
        vec![link(&symbol_id, "codegraph", "FAILED_ON")],
    );
    // Importer shape: Failure --FAILED_ON--> PatchArtifact, and
    // Failure --FAILED_ON--> CommandRun (exit_code nonzero, no status).
    let patch_id = artifact_stable_id(&["patch", "rejected"]);
    let mut patch = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "patch".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        patch_status,
        patch_bytes_hash,
        ..
    } = &mut patch
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *patch_status = Some("invalid".to_owned());
        *patch_bytes_hash = Some("blake3:ph".to_owned());
    }
    let cmd_id = verification_stable_id(&["v", "failed_cmd"]);
    let cmd = verification_node(
        &cmd_id,
        NodeKind::CommandRun,
        None, // no status — only exit_code (#7)
        Some(2),
        None,
        Some("2026-01-01T00:00:01Z"),
        vec![],
    );
    let patch_edge = GraphRecord::edge(
        EdgeLabel::FailedOn,
        fail_id.clone(),
        patch_id.clone(),
        None,
        "failure failed on patch".to_owned(),
    );
    let cmd_edge = GraphRecord::edge(
        EdgeLabel::FailedOn,
        fail_id,
        cmd_id.clone(),
        None,
        "failure failed on command".to_owned(),
    );
    let (_t, graph) = write_graph(vec![symbol, fail, patch, cmd, patch_edge, cmd_edge]);
    let (code, stdout, stderr) = run_graph(&graph, &symbol_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let patches = v["patch_artifacts"].as_array().unwrap();
    assert_eq!(patches.len(), 1, "FAILED_ON patch must be collected (#3)");
    assert_eq!(patches[0]["record_id"], patch_id);
    let runtime = v["runtime_failures"].as_array().unwrap();
    assert_eq!(runtime.len(), 1, "exit_code CommandRun must surface (#7)");
    assert_eq!(runtime[0]["record_id"], cmd_id);
}

#[test]
fn undated_failure_emits_missing_timestamp_and_still_failing() {
    let (symbol_id, symbol) = symbol_node("src/c.rs", "gamma");
    let fail_id = agent_memory_stable_id(&["failure", "undated"]);
    let fail = failure_node(
        &fail_id,
        "command_failure",
        None, // no observed_at (#4)
        vec![link(&symbol_id, "codegraph", "FAILED_ON")],
    );
    let (_t, graph) = write_graph(vec![symbol, fail]);
    let (code, stdout, _e) = run_graph(&graph, &symbol_id, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["agent_failures"][0]["resolution_status"], "still_failing");
    let codes: Vec<&str> = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"missing_timestamp"), "codes={codes:?}");
}

#[test]
fn tombstoned_failed_on_edge_is_skipped() {
    let (symbol_id, symbol) = symbol_node("src/d.rs", "delta");
    let fail_id = agent_memory_stable_id(&["failure", "edge_only"]);
    // Failure reached ONLY via a FAILED_ON edge (no denormalized link).
    let fail = failure_node(
        &fail_id,
        "command_failure",
        Some("2026-01-01T00:00:00Z"),
        vec![],
    );
    let edge = GraphRecord::edge(
        EdgeLabel::FailedOn,
        fail_id,
        symbol_id.clone(),
        None,
        "failure failed on symbol".to_owned(),
    );
    let edge_id = edge.id().to_owned();
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &edge_id]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: edge_id,
        summary: "retracted edge".to_owned(),
        producer: None,
    };
    let (_t, graph) = write_graph(vec![symbol, fail, edge, tombstone]);
    let (code, stdout, stderr) = run_graph(&graph, &symbol_id, &[]);
    // The symbol still resolves, but the retracted FAILED_ON edge must not
    // surface the failure (#6): an honest empty answer, not a stale hit.
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["agent_failures"].as_array().unwrap().len(),
        0,
        "failure reached only through a retracted edge must be excluded"
    );
}

// ── Round-2 review-fix coverage ─────────────────────────────────────────────

fn parse(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim()).expect("valid JSON")
}

fn tombstone_for(deleted_id: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: stable_id(&["tombstone", deleted_id]),
        schema_version: PROJECT_SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: "deleted".to_owned(),
        producer: None,
    }
}

#[test]
fn zero_exit_command_run_supersedes_failure() {
    // A later status-absent CommandRun with exit_code 0 is citable successful
    // evidence that supersedes an earlier failure on the same target.
    let (symbol_id, symbol) = symbol_node("src/z.rs", "zeta");
    let fail = failure_node(
        &agent_memory_stable_id(&["failure", "z1"]),
        "command_failure",
        Some("2026-01-01T00:00:00Z"),
        vec![link(&symbol_id, "codegraph", "FAILED_ON")],
    );
    let pass = verification_node(
        &verification_stable_id(&["v", "z_pass"]),
        NodeKind::CommandRun,
        None,    // no status
        Some(0), // exit_code 0 → pass (symmetry with failure classification)
        None,
        Some("2026-02-01T00:00:00Z"),
        vec![link(&symbol_id, "codegraph", "VALIDATED_BY")],
    );
    let (_t, graph) = write_graph(vec![symbol, fail, pass]);
    let (code, stdout, _e) = run_graph(&graph, &symbol_id, &[]);
    assert_eq!(code, 0);
    let v = parse(&stdout);
    assert_eq!(v["superseding_successes"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["agent_failures"][0]["resolution_status"],
        "since_resolved"
    );
}

#[test]
fn tombstoned_file_path_exits_2_stale() {
    let file_path = "src/gone.rs";
    let file_id = stable_id(&["node", "File", file_path]);
    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        file_path.to_owned(),
        span(1, 5),
        "gone.rs".to_owned(),
        "rust",
        "deleted file".to_owned(),
    );
    let tombstone = tombstone_for(&file_id);
    let (_t, graph) = write_graph(vec![file, tombstone]);
    let (code, stdout, _e) = run_graph(&graph, file_path, &[]);
    assert_eq!(code, 2, "a deleted file path must be stale, not no_match");
    assert_eq!(parse(&stdout)["error"]["code"], "stale_handle");
}

#[test]
fn task_supersession_follows_acceptance_criterion_closure_edge() {
    let task_id = project_stable_id(&["task", "t_close"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Task".to_owned()),
        "Task".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut task {
        *schema_version = PROJECT_SCHEMA_VERSION;
    }
    let ac_id = project_stable_id(&["ac", "ac_close"]);
    let mut ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC".to_owned()),
        "AC".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        parent_task_id,
        ..
    } = &mut ac
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *parent_task_id = Some(task_id.clone());
    }
    // A failure on the AC, then a passing run that CLOSES the AC (AC -> Verification).
    let fail = failure_node(
        &agent_memory_stable_id(&["failure", "ac_fail"]),
        "command_failure",
        Some("2026-01-01T00:00:00Z"),
        vec![link(&ac_id, "project", "FAILED_ON")],
    );
    let pass_id = verification_stable_id(&["v", "ac_pass"]);
    let pass = verification_node(
        &pass_id,
        NodeKind::TestRun,
        Some("pass"),
        None,
        Some("2026-02-01T00:00:00Z"),
        None,
        vec![],
    );
    let closes = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_id,
        pass_id,
        None,
        "AC closed by passing run".to_owned(),
    );
    let (_t, graph) = write_graph(vec![task, ac, fail, pass, closes]);
    let (code, stdout, stderr) = run_graph(&graph, &task_id, &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse(&stdout);
    assert_eq!(
        v["superseding_successes"].as_array().unwrap().len(),
        1,
        "AC-closing pass must surface as a superseding success"
    );
    assert_eq!(
        v["agent_failures"][0]["resolution_status"],
        "since_resolved"
    );
}

#[test]
fn unattributed_match_makes_repo_ambiguous() {
    // repo-a owns one `widget`; a second unattributed `widget` has no repository.
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    let owned_path = "a/src/lib.rs";
    let owned_file = stable_id(&["node", "File", owned_path]);
    let (owned_sym, owned_sym_node) = symbol_node(owned_path, "widget");
    let (_unattr_sym, unattr_sym_node) = symbol_node("legacy/lib.rs", "widget");
    let records = vec![
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("repo-a".to_owned()),
            "repo".to_owned(),
        ),
        GraphRecord::syntax_node(
            owned_file.clone(),
            NodeKind::File,
            owned_path.to_owned(),
            span(1, 10),
            "lib.rs".to_owned(),
            "rust",
            "f".to_owned(),
        ),
        owned_sym_node,
        unattr_sym_node,
        GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id,
            owned_file.clone(),
            None,
            "contains".to_owned(),
        ),
        GraphRecord::edge(
            EdgeLabel::Defines,
            owned_file,
            owned_sym,
            None,
            "defines".to_owned(),
        ),
    ];
    let (_t, graph) = write_graph(records);
    let (code, _stdout, stderr) = run_graph(&graph, "widget", &[]);
    assert_eq!(
        code, 1,
        "an unattributed match alongside a repo-owned one is ambiguous"
    );
    assert!(parse(&stderr).get("Ambiguous").is_some());
}

#[test]
fn source_handle_returns_only_matched_seed_failures() {
    let (symbol_id, symbol) = symbol_node("src/s.rs", "sigma");
    let make_fail = |key: &str, src: &str, when: &str| -> GraphRecord {
        let mut f = failure_node(
            &agent_memory_stable_id(&["failure", key]),
            "command_failure",
            Some(when),
            vec![link(&symbol_id, "codegraph", "FAILED_ON")],
        );
        if let GraphRecord::Node { source_handle, .. } = &mut f {
            *source_handle = Some(src.to_owned());
        }
        f
    };
    let f1 = make_fail("s1", "trajectories/run-A.traj", "2026-01-01T00:00:00Z");
    let f1_id = f1.id().to_owned();
    let f2 = make_fail("s2", "trajectories/run-B.traj", "2026-01-02T00:00:00Z");
    let (_t, graph) = write_graph(vec![symbol, f1, f2]);
    // Query by F1's source handle: only F1 is a prior attempt, not F2 (a
    // different session on the same symbol).
    let (code, stdout, stderr) = run_graph(&graph, "trajectories/run-A.traj", &[]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let v = parse(&stdout);
    assert_eq!(v["target_type"], "source");
    let agent = v["agent_failures"].as_array().unwrap();
    assert_eq!(agent.len(), 1, "source handle names its own failures only");
    assert_eq!(agent[0]["record_id"], f1_id);
}
