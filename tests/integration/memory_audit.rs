#![allow(missing_docs)]

//! End-to-end tests for `eg query memory` — the memory evidence audit query
//! (issue #64). The audit starts from one agent-authored memory claim and
//! returns its provenance, supporting / contradicting / superseding evidence,
//! related code and project handles, and verification evidence — never raw
//! payloads, and never an agent claim presented as source truth.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, OutputHandle, PROJECT_SCHEMA_VERSION,
        VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, project_stable_id, stable_id,
        verification_stable_id,
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

/// Sentinel payloads that must NEVER appear in audit output (AC9).
const RAW_STDOUT_SENTINEL: &str = "RAW_STDOUT_SHOULD_NOT_LEAK";
const RAW_PATCH_SENTINEL: &str = "RAW_PATCH_SHOULD_NOT_LEAK";
/// The claim body must be referenced by hash, never copied verbatim (AC9).
const RAW_CLAIM_TEXT_SENTINEL: &str = "RAW_CLAIM_TEXT_SHOULD_NOT_LEAK";

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    claim_id: String,
    contra_id: String,
    decision_id: String,
    stale_id: String,
    source_artifact_path: String,
}

/// Seeds a graph JSONL fixture exercising every record class named in AC1:
/// one verified observation (the claim), one unverified observation
/// (contradicting), one decision (superseding), a contradiction, a
/// supersession, a verification record, a task + acceptance criterion, a code
/// file + symbol handle, a source artifact handle, and protected/redacted
/// payloads.
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("memory_audit_seeded.jsonl");

    let mut graph = Graph::new();

    // ── Agent + session provenance ──────────────────────────────────────────
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
        ref mut schema_version,
        agent_id: ref mut aid,
        ..
    } = agent
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
        "Session sess_1 for agent agent_1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        session_id: ref mut sid,
        ..
    } = session
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *sid = Some("sess_1".to_owned());
    }

    // ── Code handles ────────────────────────────────────────────────────────
    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    );

    let symbol_id = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
    let symbol = GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        "src/lib.rs".to_owned(),
        span(10, 20),
        "foo".to_owned(),
        "rust",
        "Symbol foo".to_owned(),
    );

    // ── Project handles ─────────────────────────────────────────────────────
    let task_id = project_stable_id(&["task", "task_64"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Expose memory evidence audit".to_owned()),
        "Task #64".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ..
    } = task
    {
        *title = Some("Expose memory evidence audit".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    let ac_id = project_stable_id(&["acceptance_criterion", "ac_64_1"]);
    let mut ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("Audit returns deterministic JSON".to_owned()),
        "AC 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut status,
        ref mut parent_task_id,
        ref mut schema_version,
        ..
    } = ac
    {
        *status = Some("verified".to_owned());
        *parent_task_id = Some(task_id.clone());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    // ── Verification evidence (makes the claim "verified") ──────────────────
    let ver_id = verification_stable_id(&["verification", "ver_64"]);
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification pass".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ref mut verification_kind,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("command_run".to_owned());
    }

    // ── Protected command evidence (raw stdout must not leak) ───────────────
    let cmd_id = verification_stable_id(&["command_evidence", "cmd_64"]);
    let mut cmd = GraphRecord::node(
        cmd_id.clone(),
        NodeKind::CommandEvidence,
        None,
        None,
        None,
        "cargo test command evidence".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut stdout_handle,
        ref mut evidence_quality,
        ref mut source_artifact_path,
        ..
    } = cmd
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some(RAW_STDOUT_SENTINEL.to_owned()),
            hash: "blake3:stdouthash".to_owned(),
            bytes: 4096,
        }));
        *evidence_quality = Some("verbatim".to_owned());
        *source_artifact_path = Some("ci/test.sh".to_owned());
    }

    // ── Protected patch artifact (raw patch bytes must not leak) ────────────
    let patch_id = agent_memory_stable_id(&["artifact", "patch_64"]);
    let mut patch_artifact = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "Patch artifact for foo".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut patch_status,
        ref mut patch_bytes_hash,
        ref mut patch_handle,
        ..
    } = patch_artifact
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *patch_status = Some("valid".to_owned());
        *patch_bytes_hash = Some("blake3:patchhash".to_owned());
        *patch_handle = Some(Box::new(aletheia_egregore::ir::PatchHandle {
            path: "protected/patch_64.patch".to_owned(),
            inline: Some(RAW_PATCH_SENTINEL.to_owned()),
        }));
    }

    // ── The memory claim: a verified Observation ────────────────────────────
    let claim_id = agent_memory_stable_id(&["obs", "claim_64"]);
    let missing_target = format!("agent_memory:v1:{}", "0".repeat(64));
    let source_artifact_path = "trajectories/run-1.traj".to_owned();
    let mut claim = GraphRecord::node(
        claim_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Refactored foo for clarity".to_owned(),
    );
    let decision_id = agent_memory_stable_id(&["decision", "dec_64"]);
    if let GraphRecord::Node {
        ref mut text,
        agent_id: ref mut aid,
        ref mut agent_kind,
        session_id: ref mut sid,
        ref mut observed_at,
        ref mut ingested_at,
        ref mut confidence,
        ref mut source_handle,
        source_artifact_path: ref mut sap,
        ref mut source_artifact_hash,
        ref mut redaction_policy_version,
        ref mut superseded_by,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = claim
    {
        *text = Some(format!(
            "Refactored foo; {RAW_CLAIM_TEXT_SENTINEL} <REDACTED:secret:abcd1234>"
        ));
        *aid = Some("agent_1".to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-03T12:00:00Z".to_owned());
        *ingested_at = Some("2026-06-03T12:00:01Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *source_handle = Some("src/lib.rs:sha256:deadbeef".to_owned());
        *sap = Some(source_artifact_path.clone());
        *source_artifact_hash = Some("blake3:trajhash1".to_owned());
        *redaction_policy_version = Some("1".to_owned());
        *superseded_by = Some(decision_id.clone());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            link(&file_id, "codegraph", "OBSERVES"),
            link(&symbol_id, "codegraph", "MENTIONS_SYMBOL"),
            link(&ver_id, "verification", "VALIDATED_BY"),
            link(&cmd_id, "verification", "HAS_EVIDENCE"),
            link(&task_id, "project", "REFERENCES_TASK"),
            link(&patch_id, "artifact", "PRODUCED_PATCH"),
            // Unresolved: target does not exist in the store (AC6).
            link(&missing_target, "agent_memory", "RELATES_TO"),
        ]);
    }

    // ── Contradicting record: an UNVERIFIED Observation ─────────────────────
    let contra_id = agent_memory_stable_id(&["obs", "contra_64"]);
    let mut contra = GraphRecord::node(
        contra_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "foo refactor introduced a regression".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut observed_at,
        ref mut confidence,
        ref mut source_handle,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = contra
    {
        *text = Some("foo refactor introduced a regression".to_owned());
        *aid = Some("agent_2".to_owned());
        *sid = Some("sess_2".to_owned());
        *observed_at = Some("2026-06-04T09:00:00Z".to_owned());
        *confidence = Some("0.6".to_owned());
        *source_handle = Some("trajectories/run-2.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        // Only cites code — no verification backing, so it is UNVERIFIED.
        *evidence_links = Some(vec![link(&file_id, "codegraph", "OBSERVES")]);
    }

    // ── Superseding record: a verified Decision ─────────────────────────────
    let mut decision = GraphRecord::node(
        decision_id.clone(),
        NodeKind::Decision,
        None,
        None,
        None,
        "Adopt foo refactor as the canonical implementation".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut observed_at,
        ref mut confidence,
        ref mut source_handle,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = decision
    {
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-05T09:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
        *source_handle = Some("trajectories/run-3.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![link(&ver_id, "verification", "VALIDATED_BY")]);
    }

    // ── Edges ───────────────────────────────────────────────────────────────
    let authored_by = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        claim_id.clone(),
        session_id.clone(),
        None,
        "claim authored by session".to_owned(),
    );
    let session_of = GraphRecord::edge(
        EdgeLabel::SessionOf,
        session_id,
        agent_id,
        None,
        "session of agent".to_owned(),
    );
    let contradicts = GraphRecord::edge(
        EdgeLabel::Contradicts,
        contra_id.clone(),
        claim_id.clone(),
        Some("0.6".to_owned()),
        "contra contradicts claim".to_owned(),
    );
    let supersedes = GraphRecord::edge(
        EdgeLabel::Supersedes,
        decision_id.clone(),
        claim_id.clone(),
        None,
        "decision supersedes claim".to_owned(),
    );
    let closes = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_id,
        ver_id,
        None,
        "AC closed by verification".to_owned(),
    );

    // ── A tombstoned (stale) memory id ──────────────────────────────────────
    let stale_id = agent_memory_stable_id(&["obs", "stale_64"]);
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &stale_id]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: stale_id.clone(),
        summary: "deleted stale observation".to_owned(),
        producer: None,
    };

    graph.push(agent);
    graph.push(session);
    graph.push(file);
    graph.push(symbol);
    graph.push(task);
    graph.push(ac);
    graph.push(ver);
    graph.push(cmd);
    graph.push(patch_artifact);
    graph.push(claim);
    graph.push(contra);
    graph.push(decision);
    graph.push(authored_by);
    graph.push(session_of);
    graph.push(contradicts);
    graph.push(supersedes);
    graph.push(closes);
    graph.push(tombstone);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    Fixture {
        _temp: temp,
        graph: path,
        claim_id,
        contra_id,
        decision_id,
        stale_id,
        source_artifact_path,
    }
}

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

fn run_audit(fx: &Fixture, extra: &[&str]) -> (i32, String, String) {
    let args = vec!["query", "memory", fx.claim_id.as_str(), "--graph"];
    let assert = egregore().args(&args).arg(&fx.graph).args(extra).assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("utf8"),
        String::from_utf8(output.stderr).expect("utf8"),
    )
}

#[test]
fn audit_by_canonical_id_separates_trust_classes() {
    let fx = seed();
    let (code, stdout, _stderr) = run_audit(&fx, &[]);
    assert_eq!(code, 0, "stdout={stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(v["ok"], true);
    assert_eq!(v["memory_id"], fx.claim_id);

    // Memory claim is present and tagged as an agent-authored claim — never
    // source truth (AC3).
    let claim = v["memory_claim"].as_array().expect("claim array");
    assert_eq!(claim.len(), 1);
    assert_eq!(claim[0]["record_id"], fx.claim_id);
    assert_eq!(claim[0]["trust_class"], "agent_authored");

    // Direct provenance carries citable handles (AC3, AC4).
    let provenance = &v["direct_provenance"];
    assert_eq!(provenance["agent_id"], "agent_1");
    assert_eq!(provenance["session_id"], "sess_1");
    assert_eq!(provenance["source_handle"], "src/lib.rs:sha256:deadbeef");

    // Related code handles (AC3): file + symbol, tagged source_fact.
    let code_handles = v["related_code_handles"].as_array().expect("code handles");
    assert!(
        code_handles
            .iter()
            .any(|h| h["repo_relative_path"] == "src/lib.rs")
    );
    assert!(
        code_handles
            .iter()
            .all(|h| h["trust_class"] == "source_fact")
    );

    // Related project handles (AC3): the task.
    let projects = v["related_project_handles"].as_array().expect("project");
    assert!(projects.iter().any(|h| h["kind"] == "Task"));

    // Verification evidence (AC3): the Verification record.
    let ver = v["verification_evidence"].as_array().expect("verification");
    assert!(ver.iter().any(|h| h["kind"] == "Verification"));

    // Contradicting evidence (AC3, AC7): the unverified observation, separate.
    let contra = v["contradicting_evidence"].as_array().expect("contra");
    assert_eq!(contra.len(), 1);
    assert_eq!(contra[0]["record_id"], fx.contra_id);

    // Superseding record (AC7): the decision, as a separate audit item.
    let sup = v["superseding_records"].as_array().expect("superseding");
    assert!(sup.iter().any(|h| h["record_id"] == fx.decision_id));

    // Unresolved evidence link surfaced as a diagnostic with its source handle (AC6).
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "unresolved_evidence_link"),
        "diagnostics={diags:?}"
    );

    // Pagination block present and deterministic (AC8).
    assert_eq!(v["page"]["has_more"], false);
}

#[test]
fn audit_never_emits_raw_payloads() {
    let fx = seed();
    let (code, stdout, _stderr) = run_audit(&fx, &[]);
    assert_eq!(code, 0);
    assert!(
        !stdout.contains(RAW_STDOUT_SENTINEL),
        "raw stdout leaked: {stdout}"
    );
    assert!(
        !stdout.contains(RAW_PATCH_SENTINEL),
        "raw patch leaked: {stdout}"
    );
    // The claim's raw text body is never copied verbatim — only hashed.
    assert!(
        !stdout.contains(RAW_CLAIM_TEXT_SENTINEL),
        "raw claim text leaked: {stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let claim = &v["memory_claim"][0];
    assert!(
        claim.get("text").is_none(),
        "raw text field must not be present"
    );
    assert!(
        claim["text_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("blake3:")),
        "claim body must be referenced by a blake3 hash handle"
    );
    // Protected payloads are still acknowledged via diagnostics carrying hashes.
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    assert!(diags.iter().any(|d| d["code"] == "protected_payload"));
}

#[test]
fn audit_resolves_source_artifact_handle() {
    let fx = seed();
    let output = egregore()
        .args(["query", "memory", &fx.source_artifact_path, "--graph"])
        .arg(&fx.graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["memory_id"], fx.claim_id);
}

#[test]
fn audit_verified_only_excludes_unverified_observations() {
    let fx = seed();
    let (code, stdout, _stderr) = run_audit(&fx, &["--verified-only"]);
    assert_eq!(code, 0, "stdout={stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // The unverified contradicting observation is excluded from its section …
    let contra = v["contradicting_evidence"].as_array().expect("contra");
    assert!(contra.is_empty(), "unverified contra should be excluded");

    // … but reported in `excluded`, never silently dropped (AC5).
    let excluded = v["excluded"].as_array().expect("excluded");
    assert!(
        excluded
            .iter()
            .any(|e| e["record_id"] == fx.contra_id && e["reason"] == "unverified_observation"),
        "excluded={excluded:?}"
    );

    // The verified superseding decision survives the filter.
    let sup = v["superseding_records"].as_array().expect("superseding");
    assert!(sup.iter().any(|h| h["record_id"] == fx.decision_id));
}

#[test]
fn audit_unsupported_handle_exits_1() {
    let fx = seed();
    let assert = egregore()
        .args(["query", "memory", "agent_memory:v1:nothex", "--graph"])
        .arg(&fx.graph)
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("Unsupported"), "stderr={stderr}");
}

#[test]
fn audit_ambiguous_handle_exits_1() {
    // The fixture's Agent authored two claims (the verified observation and the
    // decision), so resolving by the Agent's canonical ID is ambiguous (AC2).
    let fx = seed();
    let agent_id = agent_memory_stable_id(&["node", "agent", "agent_1"]);
    let assert = egregore()
        .args(["query", "memory", &agent_id, "--graph"])
        .arg(&fx.graph)
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(stderr.contains("Ambiguous"), "stderr={stderr}");
}

#[test]
fn audit_missing_handle_exits_2() {
    let fx = seed();
    let absent = format!("agent_memory:v1:{}", "a".repeat(64));
    let assert = egregore()
        .args(["query", "memory", &absent, "--graph"])
        .arg(&fx.graph)
        .assert()
        .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_match");
}

#[test]
fn audit_stale_handle_reports_stale_diagnostic() {
    let fx = seed();
    let assert = egregore()
        .args(["query", "memory", &fx.stale_id, "--graph"])
        .arg(&fx.graph)
        .assert()
        .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "stale_handle");
    assert_eq!(v["error"]["memory_handle"], fx.stale_id);
}

#[test]
fn audit_is_deterministic_across_runs() {
    let fx = seed();
    let (_c0, first, _e0) = run_audit(&fx, &[]);
    for _ in 0..4 {
        let (_c, again, _e) = run_audit(&fx, &[]);
        assert_eq!(first, again, "audit output is not deterministic");
    }
}

/// Writes a small graph fixture and returns its path.
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

fn audit_json(graph: &std::path::Path, handle: &str) -> serde_json::Value {
    let output = egregore()
        .args(["query", "memory", handle, "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(output).expect("utf8").trim()).expect("valid JSON")
}

/// A denormalized `CONTRADICTS` evidence link (no graph edge) must land its
/// target in `contradicting_evidence`, never generic `supporting_evidence`.
#[test]
fn audit_denormalized_contradicts_link_routes_to_contradicting() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_d"]);
    let contra_id = agent_memory_stable_id(&["obs", "contra_d"]);

    let mut claim = GraphRecord::node(
        claim_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "claim".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut source_handle,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = claim
    {
        *aid = Some("a".to_owned());
        *sid = Some("s".to_owned());
        *source_handle = Some("trajectories/run.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        // The contradiction exists ONLY as a denormalized evidence link.
        *evidence_links = Some(vec![link(&contra_id, "agent_memory", "CONTRADICTS")]);
    }

    let mut contra = GraphRecord::node(
        contra_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "contradicting claim".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut source_handle,
        ref mut schema_version,
        ..
    } = contra
    {
        *aid = Some("b".to_owned());
        *sid = Some("s2".to_owned());
        *source_handle = Some("trajectories/run2.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }

    let (_t, graph) = write_graph(vec![claim, contra]);
    let v = audit_json(&graph, &claim_id);

    let contradicting = v["contradicting_evidence"].as_array().expect("contra");
    assert!(
        contradicting.iter().any(|h| h["record_id"] == contra_id),
        "denormalized CONTRADICTS link not routed to contradicting_evidence: {v}"
    );
    let supporting = v["supporting_evidence"].as_array().expect("support");
    assert!(
        !supporting.iter().any(|h| h["record_id"] == contra_id),
        "contradiction must not appear as supporting evidence"
    );
}

/// An `AgentSession` canonical ID resolves its scoped claim via the session
/// node's `session_id` field even when `name` holds a human summary.
#[test]
fn audit_session_scope_resolves_by_session_id_field() {
    let session_id_node = agent_memory_stable_id(&["node", "agent_session", "a", "real-key"]);
    let claim_id = agent_memory_stable_id(&["obs", "claim_s"]);

    let mut session = GraphRecord::node(
        session_id_node.clone(),
        NodeKind::AgentSession,
        None,
        None,
        // `name` is a human summary, NOT the session key.
        Some("Codex session: refactor sweep".to_owned()),
        "session".to_owned(),
    );
    if let GraphRecord::Node {
        session_id: ref mut sid,
        ref mut schema_version,
        ..
    } = session
    {
        *sid = Some("real-key".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }

    let mut claim = GraphRecord::node(
        claim_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "claim".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut source_handle,
        ref mut schema_version,
        ..
    } = claim
    {
        *aid = Some("a".to_owned());
        *sid = Some("real-key".to_owned());
        *source_handle = Some("trajectories/run.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }

    let (_t, graph) = write_graph(vec![session, claim]);
    let v = audit_json(&graph, &session_id_node);
    assert_eq!(v["ok"], true);
    assert_eq!(v["memory_id"], claim_id);
}

/// A resolved memory ID that is tombstoned is stale even when the original
/// node is still present in an incremental graph.
#[test]
fn audit_stale_when_node_and_tombstone_both_present() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_t"]);
    let mut claim = GraphRecord::node(
        claim_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "claim".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut source_handle,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = claim
    {
        *aid = Some("a".to_owned());
        *sid = Some("s".to_owned());
        *source_handle = Some("trajectories/run.traj".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![link(
            &stable_id(&["node", "File", "src/lib.rs"]),
            "codegraph",
            "OBSERVES",
        )]);
    }
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &claim_id]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: claim_id.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };

    let (_t, graph) = write_graph(vec![claim, tombstone]);
    let assert = egregore()
        .args(["query", "memory", &claim_id, "--graph"])
        .arg(&graph)
        .assert()
        .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["error"]["code"], "stale_handle", "{v}");
}

/// Builds an agent-memory node stamped with the agent-memory schema version.
fn agent_node(id: &str, kind: NodeKind, name: Option<&str>) -> GraphRecord {
    let mut n = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        name.map(ToOwned::to_owned),
        "node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = n
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    n
}

/// Builds an `Observation` claim node with provenance and evidence links.
fn mk_obs(id: &str, source_handle: &str, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut n = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "obs".to_owned(),
    );
    if let GraphRecord::Node {
        agent_id: ref mut a,
        session_id: ref mut s,
        source_handle: ref mut sh,
        schema_version: ref mut sv,
        evidence_links: ref mut el,
        ..
    } = n
    {
        *a = Some("a".to_owned());
        *s = Some("s".to_owned());
        *sh = Some(source_handle.to_owned());
        *sv = AGENT_MEMORY_SCHEMA_VERSION;
        if !links.is_empty() {
            *el = Some(links);
        }
    }
    n
}

/// `superseded_by` is read even when the claim carries no `evidence_links`.
#[test]
fn audit_reads_superseded_by_without_evidence_links() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_sup"]);
    let decision_id = agent_memory_stable_id(&["decision", "dec_sup"]);

    let mut claim = mk_obs(&claim_id, "trajectories/run.traj", vec![]);
    if let GraphRecord::Node {
        ref mut superseded_by,
        ..
    } = claim
    {
        *superseded_by = Some(decision_id.clone());
    }
    let decision = agent_node(&decision_id, NodeKind::Decision, None);

    let (_t, graph) = write_graph(vec![claim, decision]);
    let v = audit_json(&graph, &claim_id);
    let sup = v["superseding_records"].as_array().expect("superseding");
    assert!(
        sup.iter().any(|h| h["record_id"] == decision_id),
        "superseded_by not read without evidence_links: {v}"
    );
}

/// A live source handle that also matches a tombstoned claim is not ambiguous.
#[test]
fn audit_live_handle_not_ambiguous_with_tombstoned_twin() {
    let live_id = agent_memory_stable_id(&["obs", "live"]);
    let dead_id = agent_memory_stable_id(&["obs", "dead"]);
    let live = mk_obs(&live_id, "shared.traj", vec![]);
    let dead = mk_obs(&dead_id, "shared.traj", vec![]);
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &dead_id]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: dead_id.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };

    let (_t, graph) = write_graph(vec![live, dead, tombstone]);
    let v = audit_json(&graph, "shared.traj");
    assert_eq!(v["ok"], true, "live handle should not be ambiguous: {v}");
    assert_eq!(v["memory_id"], live_id);
}

/// A `--verified-only` claim backed only by a generic `RELATES_TO` link to a
/// verification record is treated as unverified and excluded.
#[test]
fn audit_verified_only_requires_backing_relation() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_b"]);
    let contra_id = agent_memory_stable_id(&["obs", "contra_b"]);
    let ver_id = verification_stable_id(&["verification", "ver_b"]);

    let claim = mk_obs(&claim_id, "run.traj", vec![]);
    // The contradicting obs only RELATES_TO a verification record — not backing.
    let contra = mk_obs(
        &contra_id,
        "run2.traj",
        vec![link(&ver_id, "verification", "RELATES_TO")],
    );
    let mut ver = GraphRecord::node(
        ver_id,
        NodeKind::Verification,
        None,
        None,
        None,
        "ver".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
    }
    let contradicts = GraphRecord::edge(
        EdgeLabel::Contradicts,
        contra_id.clone(),
        claim_id.clone(),
        None,
        "c".to_owned(),
    );

    let (_t, graph) = write_graph(vec![claim, contra, ver, contradicts]);
    let output = egregore()
        .args(["query", "memory", &claim_id, "--graph"])
        .arg(&graph)
        .args(["--verified-only"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).expect("json");
    let excluded = v["excluded"].as_array().expect("excluded");
    assert!(
        excluded.iter().any(|e| e["record_id"] == contra_id),
        "RELATES_TO to a verification record must not count as verified: {v}"
    );
}

/// An evidence link to a tombstoned target is reported stale, not surfaced as
/// live verification evidence.
#[test]
fn audit_tombstoned_evidence_target_is_stale() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_te"]);
    let ver_id = verification_stable_id(&["verification", "ver_te"]);
    let claim = mk_obs(
        &claim_id,
        "run.traj",
        vec![link(&ver_id, "verification", "VALIDATED_BY")],
    );
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "ver".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
    }
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &ver_id]),
        schema_version: VERIFICATION_SCHEMA_VERSION,
        deleted_id: ver_id.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };

    let (_t, graph) = write_graph(vec![claim, ver, tombstone]);
    let v = audit_json(&graph, &claim_id);
    let ver_ev = v["verification_evidence"].as_array().expect("ver");
    assert!(
        !ver_ev.iter().any(|h| h["record_id"] == ver_id),
        "tombstoned evidence must not appear as live verification: {v}"
    );
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "stale_evidence_target" && d["target_handle"] == ver_id),
        "tombstoned target should be reported stale: {v}"
    );
}

/// Provenance is resolved through an `AUTHORED_BY` → `AgentRun` → `SESSION_OF`
/// chain, surfacing the real `AgentSession` rather than an intermediate turn.
#[test]
fn audit_resolves_authored_by_chain_to_session() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_ch"]);
    let turn_id = agent_memory_stable_id(&["turn", "t"]);
    let run_id = agent_memory_stable_id(&["run", "r"]);
    let session_node = agent_memory_stable_id(&["session", "real"]);

    let claim = mk_obs(&claim_id, "run.traj", vec![]);
    let turn = agent_node(&turn_id, NodeKind::AgentTurn, None);
    let run = agent_node(&run_id, NodeKind::AgentRun, None);
    let session = agent_node(&session_node, NodeKind::AgentSession, Some("sess"));
    let e1 = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        claim_id.clone(),
        turn_id.clone(),
        None,
        "claim->turn".to_owned(),
    );
    let e2 = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        turn_id,
        run_id.clone(),
        None,
        "turn->run".to_owned(),
    );
    let e3 = GraphRecord::edge(
        EdgeLabel::SessionOf,
        run_id,
        session_node.clone(),
        None,
        "run->session".to_owned(),
    );

    let (_t, graph) = write_graph(vec![claim, turn, run, session, e1, e2, e3]);
    let v = audit_json(&graph, &claim_id);
    let sessions = v["direct_provenance"]["agent_session_ids"]
        .as_array()
        .expect("agent_session_ids");
    assert!(
        sessions.iter().any(|s| s == &session_node),
        "real AgentSession not surfaced from AUTHORED_BY chain: {v}"
    );
}

/// A direct `RELATES_TO` edge (no denormalized link) surfaces its target as
/// related evidence rather than dropping it.
#[test]
fn audit_direct_relates_to_edge_is_surfaced() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_rel"]);
    let ver_id = verification_stable_id(&["verification", "ver_rel"]);
    let claim = mk_obs(&claim_id, "run.traj", vec![]);
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification pass".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
    }
    let edge = GraphRecord::edge(
        EdgeLabel::RelatesTo,
        claim_id.clone(),
        ver_id.clone(),
        None,
        "relates".to_owned(),
    );

    let (_t, graph) = write_graph(vec![claim, ver, edge]);
    let v = audit_json(&graph, &claim_id);
    let ver_ev = v["verification_evidence"].as_array().expect("ver");
    assert!(
        ver_ev.iter().any(|h| h["record_id"] == ver_id),
        "direct RELATES_TO edge target not surfaced: {v}"
    );
}

/// A source/session handle that matches only a tombstoned claim reports
/// `stale_handle`, not `no_match`.
#[test]
fn audit_source_handle_for_deleted_claim_is_stale() {
    let dead_id = agent_memory_stable_id(&["obs", "dead_src"]);
    let dead = mk_obs(&dead_id, "only-dead.traj", vec![]);
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &dead_id]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: dead_id.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };

    let (_t, graph) = write_graph(vec![dead, tombstone]);
    let assert = egregore()
        .args(["query", "memory", "only-dead.traj", "--graph"])
        .arg(&graph)
        .assert()
        .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["error"]["code"], "stale_handle", "{v}");
}

/// An agent-authored evidence item never forwards its stored summary verbatim
/// (the importer embeds a text prefix there); it is synthesized + hashed.
#[test]
fn audit_agent_summary_is_not_emitted_verbatim() {
    const SUMMARY_SENTINEL: &str = "SUMMARY_SNIPPET_SHOULD_NOT_LEAK";
    let claim_id = agent_memory_stable_id(&["obs", "claim_sum"]);
    let contra_id = agent_memory_stable_id(&["obs", "contra_sum"]);
    let claim = mk_obs(&claim_id, "run.traj", vec![]);
    let mut contra = mk_obs(&contra_id, "run2.traj", vec![]);
    if let GraphRecord::Node {
        ref mut summary, ..
    } = contra
    {
        *summary = format!("Observation by a in s: {SUMMARY_SENTINEL}");
    }
    let edge = GraphRecord::edge(
        EdgeLabel::Contradicts,
        contra_id.clone(),
        claim_id.clone(),
        None,
        "c".to_owned(),
    );

    let (_t, graph) = write_graph(vec![claim, contra, edge]);
    let output = egregore()
        .args(["query", "memory", &claim_id, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        !stdout.contains(SUMMARY_SENTINEL),
        "agent summary snippet leaked: {stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let contra_item = v["contradicting_evidence"]
        .as_array()
        .expect("contra")
        .iter()
        .find(|h| h["record_id"] == contra_id)
        .expect("contra item present");
    assert!(
        contra_item["summary_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("blake3:")),
        "agent-authored item should carry a summary_hash: {contra_item}"
    );
}

/// A tombstoned `AgentSession` scope handle is stale even when a live claim
/// shares its session key.
#[test]
fn audit_tombstoned_scope_handle_is_stale() {
    let session_node = agent_memory_stable_id(&["session", "deleted"]);
    let claim_id = agent_memory_stable_id(&["obs", "claim_scope"]);
    let mut session = agent_node(&session_node, NodeKind::AgentSession, Some("sess"));
    if let GraphRecord::Node {
        session_id: ref mut sid,
        ..
    } = session
    {
        *sid = Some("k".to_owned());
    }
    let mut claim = mk_obs(&claim_id, "run.traj", vec![]);
    if let GraphRecord::Node {
        session_id: ref mut sid,
        ..
    } = claim
    {
        *sid = Some("k".to_owned());
    }
    let tombstone = GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &session_node]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: session_node.clone(),
        summary: "deleted".to_owned(),
        producer: None,
    };

    let (_t, graph) = write_graph(vec![session, claim, tombstone]);
    let assert = egregore()
        .args(["query", "memory", &session_node, "--graph"])
        .arg(&graph)
        .assert()
        .code(2);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["error"]["code"], "stale_handle", "{v}");
}

/// A reverse denormalized `SUPERSEDES` link on a newer record (no edge) still
/// surfaces that record in `superseding_records`.
#[test]
fn audit_reverse_supersedes_link_is_surfaced() {
    let old_id = agent_memory_stable_id(&["obs", "old"]);
    let new_id = agent_memory_stable_id(&["decision", "new"]);
    let old_claim = mk_obs(&old_id, "run.traj", vec![]);
    let mut newer = agent_node(&new_id, NodeKind::Decision, None);
    if let GraphRecord::Node {
        ref mut evidence_links,
        ..
    } = newer
    {
        *evidence_links = Some(vec![link(&old_id, "agent_memory", "SUPERSEDES")]);
    }

    let (_t, graph) = write_graph(vec![old_claim, newer]);
    let v = audit_json(&graph, &old_id);
    let sup = v["superseding_records"].as_array().expect("superseding");
    assert!(
        sup.iter().any(|h| h["record_id"] == new_id),
        "reverse SUPERSEDES link not surfaced: {v}"
    );
}

/// An edge-only evidence target that is absent is surfaced as a diagnostic, not
/// silently skipped.
#[test]
fn audit_edge_only_missing_target_is_diagnosed() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_edge"]);
    let missing = verification_stable_id(&["verification", "gone"]);
    let claim = mk_obs(&claim_id, "run.traj", vec![]);
    let edge = GraphRecord::edge(
        EdgeLabel::HasEvidence,
        claim_id.clone(),
        missing.clone(),
        None,
        "has-evidence".to_owned(),
    );

    let (_t, graph) = write_graph(vec![claim, edge]);
    let v = audit_json(&graph, &claim_id);
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "unresolved_evidence_link" && d["target_handle"] == missing),
        "edge-only missing target not diagnosed: {v}"
    );
}

/// A cited `ToolCall`'s arguments/result handles are reported as protected
/// payloads by hash.
#[test]
fn audit_toolcall_handles_are_protected() {
    let claim_id = agent_memory_stable_id(&["obs", "claim_tc"]);
    let tool_id = agent_memory_stable_id(&["toolcall", "tc"]);
    let claim = mk_obs(
        &claim_id,
        "run.traj",
        vec![link(&tool_id, "agent_memory", "RELATES_TO")],
    );
    let mut tool = agent_node(&tool_id, NodeKind::ToolCall, None);
    if let GraphRecord::Node {
        ref mut arguments_handle,
        ..
    } = tool
    {
        *arguments_handle = Some(Box::new(OutputHandle {
            inline: None,
            hash: "blake3:argshash".to_owned(),
            bytes: 128,
        }));
    }

    let (_t, graph) = write_graph(vec![claim, tool]);
    let v = audit_json(&graph, &claim_id);
    let support = v["supporting_evidence"].as_array().expect("support");
    let tc = support
        .iter()
        .find(|h| h["record_id"] == tool_id)
        .expect("toolcall surfaced");
    assert_eq!(
        tc["protected"], true,
        "ToolCall should be marked protected: {v}"
    );
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "protected_payload" && d["relation"] == "tool_arguments"),
        "ToolCall arguments handle not surfaced as protected_payload: {v}"
    );
}

// ---------------------------------------------------------------------------
// PR #314 review: dependency declarations classify as code handles
// ---------------------------------------------------------------------------

/// An Observation's evidence link to a `DependencyDeclaration` record must
/// land in `related_code_handles` (the record is a deterministic code-graph
/// source fact), never in `supporting_evidence`.
#[test]
fn dependency_declaration_evidence_lands_in_related_code_handles() {
    let dep_records = aletheia_egregore::manifest_deps::manifest_dependency_records(
        "repo-x",
        "Cargo.toml",
        "[package]\nname = \"pkg\"\n\n[dependencies]\nserde = \"1\"\n",
        &aletheia_egregore::manifest_deps::LockfileStatus::Absent,
    );
    assert_eq!(dep_records.len(), 1);
    let dep_id = dep_records[0].id().to_owned();

    let claim_id = agent_memory_stable_id(&["node", "observation", "obs-dep-handle"]);
    let mut claim = GraphRecord::node(
        claim_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about a declared dependency".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        text,
        evidence_links,
        ..
    } = &mut claim
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent_1".to_owned());
        *text = Some("we already depend on serde".to_owned());
        *evidence_links = Some(vec![link(&dep_id, "codegraph", "MENTIONS_SYMBOL")]);
    }

    let mut records = dep_records;
    records.push(claim);

    let ctx = aletheia_egregore::query::memory_audit_context(&records, &claim_id, false);
    assert!(
        ctx.related_code_handles
            .iter()
            .any(|item| item.record.id() == dep_id),
        "the dependency record must classify as a related code handle"
    );
    assert!(
        !ctx.supporting_evidence
            .iter()
            .any(|item| item.record.id() == dep_id),
        "the dependency record must not fall through to supporting evidence"
    );
}
