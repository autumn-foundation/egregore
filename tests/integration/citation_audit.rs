#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::redundant_clone,
    clippy::similar_names,
    clippy::large_stack_arrays,
    clippy::large_stack_frames
)]

//! End-to-end tests for `eg audit citations` — the citation-completeness audit
//! (issue #65). The audit drives every public query workflow over a seeded
//! local record set and reports, per workflow and overall, whether returned
//! rows carry the citation handles their trust class requires.

use std::{fs, path::PathBuf, time::Instant};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, LOG_SCHEMA_VERSION, MetricKind, NodeKind,
    SelectionBasis, SourceSpan, TemporalMetadata, UserContextScope,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, ErrorSignaturePayload,
        FrameResolution, Graph, LogOccurrenceBucketPayload, LogPayload, LogSourcePayload,
        PROJECT_SCHEMA_VERSION, PatchHandle, SEMANTIC_SCHEMA_VERSION, SemanticDriftMetadata,
        USER_CONTEXT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        artifact_stable_id, project_stable_id, semantic_stable_id, stable_id,
        user_context_stable_id, verification_stable_id,
    },
    log_stable_id,
};
use assert_cmd::Command;
use serde_json::Value;

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

/// Sentinels that must NEVER appear in audit output (AC8).
const RAW_STDOUT_SENTINEL: &str = "RAW_STDOUT_SHOULD_NOT_LEAK";
const RAW_PATCH_SENTINEL: &str = "RAW_PATCH_SHOULD_NOT_LEAK";
const RAW_CLAIM_TEXT_SENTINEL: &str = "RAW_CLAIM_TEXT_SHOULD_NOT_LEAK";
const RAW_TASK_BODY_SENTINEL: &str = "RAW_TASK_BODY_SHOULD_NOT_LEAK";
// #328: a bounded, post-redaction log template excerpt is raw runtime text; it
// must never appear in the audit output (only the record ID + LogSource
// provenance handle are cited).
const RAW_LOG_EXCERPT_SENTINEL: &str = "RAW_LOG_EXCERPT_SHOULD_NOT_LEAK";

const SENTINELS: &[&str] = &[
    RAW_STDOUT_SENTINEL,
    RAW_PATCH_SENTINEL,
    RAW_CLAIM_TEXT_SENTINEL,
    RAW_TASK_BODY_SENTINEL,
    RAW_LOG_EXCERPT_SENTINEL,
];

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    protected_handle: String,
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

/// Builds a deterministic agent-memory observation with provenance.
fn observation(
    id: &str,
    agent: &str,
    session: &str,
    source_handle: &str,
    text: Option<&str>,
    links: Vec<EvidenceLink>,
) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        agent_kind,
        session_id,
        observed_at,
        ingested_at,
        confidence,
        source_handle: sh,
        text: t,
        evidence_links,
        ..
    } = &mut rec
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some(agent.to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *session_id = Some(session.to_owned());
        *observed_at = Some("2026-06-03T12:00:00Z".to_owned());
        *ingested_at = Some("2026-06-03T12:00:01Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *sh = Some(source_handle.to_owned());
        *t = text.map(str::to_owned);
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    rec
}

/// Seeds a JSONL fixture exercising every trust class named in AC1: code facts,
/// a semantic-drift result, verified and unverified agent memory, project
/// task/AC, an artifact behind a protected handle, verification evidence, a
/// redaction marker, a protected raw-artifact handle, and a user-context policy
/// chain — all wired so every public query workflow returns rows.
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("citation_audit_seeded.jsonl");
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

    // ── Code facts ──────────────────────────────────────────────────────────
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
    let defines = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        symbol_id.clone(),
        None,
        "file defines foo".to_owned(),
    );

    // ── Manifest-declared dependency facts (issue #180 / PR #314 review) ────
    // Real extractor output: a plain entry plus a `package = "…"` rename pair,
    // spanless facts cited by their repo-relative manifest handle.
    let dependency_records = aletheia_egregore::manifest_deps::manifest_dependency_records(
        "repo-fixture",
        "Cargo.toml",
        "[package]\nname = \"pkg\"\n\n[dependencies]\nembedded-hal = \"0.2\"\nembedded-hal-1 = { package = \"embedded-hal\", version = \"1\" }\n",
        &aletheia_egregore::manifest_deps::LockfileStatus::Absent,
        None,
    );
    assert_eq!(
        dependency_records.len(),
        2,
        "fixture seeds both rename-pair entries"
    );

    // ── Semantic drift result ───────────────────────────────────────────────
    let drift_id = semantic_stable_id(&["drift", "foo"]);
    let mut drift = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Semantic drift on foo".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        semantic_drift,
        ..
    } = &mut drift
    {
        *schema_version = SEMANTIC_SCHEMA_VERSION;
        *domain = Some("semantic".to_owned());
        *semantic_drift = Some(Box::new(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "aletheiadb_re_export".to_owned(),
                name: "sentence-transformers/all-MiniLM-L6-v2".to_owned(),
                version: "0.1.0".to_owned(),
                dim: 384,
                content_hash: "unknown".to_owned(),
            },
            target_record_id: symbol_id.clone(),
            prior_record_id: symbol_id.clone(),
            before_git_commit: "aaaa".to_owned(),
            after_git_commit: "bbbb".to_owned(),
            before_valid_time: "2026-06-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-06-02T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.8,
            selection_threshold: 0.4,
            selection_basis: SelectionBasis::ThresholdOnly,
        }));
    }
    let drifts_from = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.clone(),
        symbol_id.clone(),
        None,
        "drift drifts from foo".to_owned(),
    );

    // ── Project handles ─────────────────────────────────────────────────────
    let task_id = project_stable_id(&["task", "task_65"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Audit citation completeness".to_owned()),
        "Task #65".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        entity_id,
        title,
        ..
    } = &mut task
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *entity_id = Some("task-65".to_owned());
        *title = Some(format!("Audit completeness {RAW_TASK_BODY_SENTINEL}"));
    }

    let ac_id = project_stable_id(&["acceptance_criterion", "ac_65_1"]);
    let mut ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("Report is deterministic".to_owned()),
        "AC 1".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        parent_task_id,
        ..
    } = &mut ac
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *status = Some("verified".to_owned());
        *parent_task_id = Some(task_id.clone());
    }

    // ── Verification evidence ───────────────────────────────────────────────
    let ver_id = verification_stable_id(&["verification", "ver_65"]);
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification pass".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        ..
    } = &mut ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("command_run".to_owned());
    }

    let cmd_id = verification_stable_id(&["command_evidence", "cmd_65"]);
    let mut cmd = GraphRecord::node(
        cmd_id.clone(),
        NodeKind::CommandEvidence,
        None,
        None,
        None,
        "cargo test command evidence".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        stdout_handle,
        evidence_quality,
        ..
    } = &mut cmd
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *stdout_handle = Some(Box::new(aletheia_egregore::ir::OutputHandle {
            inline: Some(RAW_STDOUT_SENTINEL.to_owned()),
            hash: "blake3:stdouthash".to_owned(),
            bytes: 4096,
        }));
        *evidence_quality = Some("verbatim".to_owned());
    }

    // ── Artifact behind a protected raw-artifact handle ─────────────────────
    let protected_handle = format!("protected:v1:{}", "a".repeat(64));
    let patch_id = artifact_stable_id(&["artifact", "patch_65"]);
    let mut patch = GraphRecord::node(
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
        source_handle,
        ..
    } = &mut patch
    {
        *schema_version = ARTIFACT_SCHEMA_VERSION;
        *patch_status = Some("valid".to_owned());
        *patch_bytes_hash = Some("blake3:patchhash".to_owned());
        *source_handle = Some(protected_handle.clone());
        *patch_handle = Some(Box::new(PatchHandle {
            path: "protected/patch_65.patch".to_owned(),
            inline: Some(RAW_PATCH_SENTINEL.to_owned()),
        }));
    }

    // ── The memory claim: a verified Observation (redaction marker) ─────────
    let claim_id = agent_memory_stable_id(&["obs", "claim_65"]);
    let missing_target = format!("agent_memory:v1:{}", "0".repeat(64));
    let mut claim = observation(
        &claim_id,
        "agent_1",
        "sess_1",
        "src/lib.rs:sha256:deadbeef",
        Some(&format!(
            "Refactored foo; {RAW_CLAIM_TEXT_SENTINEL} <REDACTED:secret:abcd1234>"
        )),
        vec![
            link(&file_id, "codegraph", "OBSERVES"),
            link(&symbol_id, "codegraph", "MENTIONS_SYMBOL"),
            link(&ver_id, "verification", "VALIDATED_BY"),
            link(&cmd_id, "verification", "HAS_EVIDENCE"),
            link(&task_id, "project", "REFERENCES_TASK"),
            link(&patch_id, "artifact", "PRODUCED_PATCH"),
            link(&missing_target, "agent_memory", "RELATES_TO"),
        ],
    );
    if let GraphRecord::Node {
        redaction_policy_version,
        ..
    } = &mut claim
    {
        *redaction_policy_version = Some("v1".to_owned());
    }

    // ── Unverified contradicting observation ────────────────────────────────
    let contra_id = agent_memory_stable_id(&["obs", "contra_65"]);
    let contra = observation(
        &contra_id,
        "agent_2",
        "sess_2",
        "trajectories/run-2.traj",
        Some("foo refactor introduced a regression"),
        vec![link(&file_id, "codegraph", "OBSERVES")],
    );

    // ── Superseding verified Decision ───────────────────────────────────────
    let decision_id = agent_memory_stable_id(&["decision", "dec_65"]);
    let mut decision = GraphRecord::node(
        decision_id.clone(),
        NodeKind::Decision,
        None,
        None,
        None,
        "Adopt foo refactor".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id: aid,
        session_id: sid,
        observed_at,
        confidence,
        source_handle,
        evidence_links,
        ..
    } = &mut decision
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-05T09:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
        *source_handle = Some("trajectories/run-3.traj".to_owned());
        *evidence_links = Some(vec![link(&ver_id, "verification", "VALIDATED_BY")]);
    }

    // ── Memory edges ────────────────────────────────────────────────────────
    let authored_by = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        claim_id.clone(),
        session_id.clone(),
        None,
        "claim authored by session".to_owned(),
    );
    let session_of = GraphRecord::edge(
        EdgeLabel::SessionOf,
        session_id.clone(),
        agent_id.clone(),
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

    // ── User-context policy chain (candidate → prompt → decision → pref) ─────
    let cand_id = user_context_stable_id(&["candidate", "cand_65"]);
    let prompt_id = user_context_stable_id(&["prompt", "prompt_65"]);
    let dec_id = user_context_stable_id(&["decision", "decision_65"]);
    let pref_id = user_context_stable_id(&["preference", "pref_65"]);
    let rule_text = "Use snake_case for module names";

    // Three supporting observations across two sessions (promotion threshold).
    let promo_obs: Vec<GraphRecord> = [
        ("promo_1", "promo_sess_1"),
        ("promo_2", "promo_sess_1"),
        ("promo_3", "promo_sess_2"),
    ]
    .iter()
    .map(|(slug, sess)| {
        let id = agent_memory_stable_id(&["obs", slug]);
        observation(
            &id,
            "agent_1",
            sess,
            &format!("trajectories/{slug}.traj"),
            Some("Repeated naming observation"),
            vec![],
        )
    })
    .collect();
    let promo_ids: Vec<String> = promo_obs.iter().map(|o| o.id().to_owned()).collect();

    let mut candidate = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Promotion candidate".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        confidence,
        evidence_quality,
        user_context,
        ..
    } = &mut candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.proposed_rule_text = Some(rule_text.to_owned());
        user_context.scope = Some(UserContextScope::default());
        user_context.contradicting_evidence = Some(vec![]);
        user_context.supporting_evidence = Some(
            promo_ids
                .iter()
                .map(|id| link(id, "agent_memory", "PROPOSED_BY"))
                .collect(),
        );
    }

    let mut prompt = GraphRecord::node(
        prompt_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Promotion prompt".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = &mut prompt
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        user_context.candidate_id = Some(cand_id.clone());
        user_context.prompt_surface = Some("cli".to_owned());
        user_context.prompt_text = Some("Approve this preference?".to_owned());
        user_context.prompted_at = Some("2026-06-01T10:00:00Z".to_owned());
        user_context.prompted_to = Some("operator".to_owned());
    }

    let mut udecision = GraphRecord::node(
        dec_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Promotion decision".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = &mut udecision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        user_context.candidate_id = Some(cand_id.clone());
        user_context.prompt_id = Some(prompt_id.clone());
        user_context.outcome = Some("approved".to_owned());
        user_context.materialized_record_id = Some(pref_id.clone());
        user_context.decided_at = Some("2026-06-01T10:05:00Z".to_owned());
        user_context.decided_by = Some("operator".to_owned());
    }

    let mut pref = GraphRecord::node(
        pref_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Durable preference".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = &mut pref
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        user_context.rule_text = Some(rule_text.to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.scope = Some(UserContextScope::default());
        user_context.approval_decision_id = Some(dec_id.clone());
        user_context.active_from = Some("2026-06-01T10:05:00Z".to_owned());
    }

    // A pending (undecided) PromoteCandidate so the `candidates` workflow returns
    // a row — these escape the materialized-policy lane (issue #65 review #4).
    let pending_cand_id = user_context_stable_id(&["candidate", "pending_65"]);
    let mut pending_candidate = GraphRecord::node(
        pending_cand_id,
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Pending promotion candidate".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        confidence,
        evidence_quality,
        user_context,
        ..
    } = &mut pending_candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.8".to_owned());
        *evidence_quality = Some("summarized".to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.proposed_rule_text = Some("Prefer explicit imports".to_owned());
        user_context.scope = Some(UserContextScope::default());
        user_context.contradicting_evidence = Some(vec![]);
        user_context.supporting_evidence =
            Some(vec![link(&promo_ids[0], "agent_memory", "PROPOSED_BY")]);
    }

    // ── Log-domain runtime observations (#320/#322 + range for #326) ─────────
    // Three linear commits give `eg query log-deltas` a commit range whose
    // valid-time window is [T2, T3]; each ErrorSignature is cited by resolving
    // its CAPTURED_FROM edge to a LogSource carrying a source_artifact_hash, so
    // the runtime_observation lane passes at the strictest default gate (1.0).
    let repo_id = stable_id(&["repository", "operator-override", "audit-repo"]);
    let repo = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("audit-repo".to_owned()),
        "Repository audit-repo".to_owned(),
    );
    let commit = |sha: &str, parents: &[&str], valid_time: &str| -> GraphRecord {
        GraphRecord::node(
            stable_id(&["node", "commit", "audit-repo", sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: sha.to_owned(),
            git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
            valid_time: valid_time.to_owned(),
            author_time: Some(valid_time.to_owned()),
            observed_at: valid_time.to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        })
    };
    let c1 = commit("c1sha0000", &[], "2026-01-01T00:00:00Z");
    let c2 = commit("c2sha0000", &["c1sha0000"], "2026-01-02T00:00:00Z");
    let c3 = commit("c3sha0000", &["c2sha0000"], "2026-01-03T00:00:00Z");

    let log_source_id = log_stable_id(&["log_source", "audit-repo", "logs/app.log", "loghash1"]);
    let log_source = GraphRecord::node(
        log_source_id.clone(),
        NodeKind::LogSource,
        Some("logs/app.log".to_owned()),
        None,
        Some("logs/app.log".to_owned()),
        "Log source logs/app.log".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogSource(LogSourcePayload {
        source_relative_path: "logs/app.log".to_owned(),
        source_format_version: "plain-v1".to_owned(),
        source_artifact_hash: "loghash1".to_owned(),
        line_count: 42,
        repository_id: "audit-repo".to_owned(),
    }))
    .with_valid_time("2026-01-02T12:00:00Z", "inferred_from_transaction_time");

    let error_signature = |seed: &str, excerpt: &str, first: &str, last: &str| -> GraphRecord {
        GraphRecord::node(
            log_stable_id(&[
                "error_signature",
                "audit-repo",
                "template-v1",
                seed,
                "error",
            ]),
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            format!("Error signature: {seed}"),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
            fingerprint_algorithm: "template-v1".to_owned(),
            template_excerpt: excerpt.to_owned(),
            severity: "error".to_owned(),
            occurrence_count: 3,
            first_seen: first.to_owned(),
            last_seen: last.to_owned(),
            frames: None,
            repository_id: "audit-repo".to_owned(),
        }))
        .with_valid_time(first, "log_event_timestamp")
    };
    // A `new` signature (first_seen inside [T2, T3]) and a `continuing` one
    // (first before the window, last through/after its end). The excerpt carries
    // a raw-text sentinel that must never surface in the audit output.
    let sig_new = error_signature(
        "boom-new",
        &format!("boom {RAW_LOG_EXCERPT_SENTINEL}"),
        "2026-01-02T12:00:00Z",
        "2026-01-02T13:00:00Z",
    );
    let sig_new_id = sig_new.id().to_owned();
    let sig_cont = error_signature(
        "boom-cont",
        "recurring boom",
        "2026-01-01T00:00:00Z",
        "2026-01-05T00:00:00Z",
    );
    let sig_cont_id = sig_cont.id().to_owned();

    let captured_new = GraphRecord::edge(
        EdgeLabel::CapturedFrom,
        sig_new_id.clone(),
        log_source_id.clone(),
        None,
        "ErrorSignature captured from LogSource".to_owned(),
    );
    let captured_cont = GraphRecord::edge(
        EdgeLabel::CapturedFrom,
        sig_cont_id.clone(),
        log_source_id.clone(),
        None,
        "ErrorSignature captured from LogSource".to_owned(),
    );

    let bucket_id = log_stable_id(&[
        "log_occurrence_bucket",
        "audit-repo",
        &sig_new_id,
        "2026-01-02T12:00:00Z",
    ]);
    let bucket = GraphRecord::node(
        bucket_id.clone(),
        NodeKind::LogOccurrenceBucket,
        None,
        None,
        Some("bucket".to_owned()),
        "Occurrence bucket".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogOccurrenceBucket(
        LogOccurrenceBucketPayload {
            bucket_start: "2026-01-02T12:00:00Z".to_owned(),
            bucket_width: "1h".to_owned(),
            occurrence_count: 3,
            source_id: log_source_id.clone(),
            repository_id: "audit-repo".to_owned(),
            occurrence_timestamps: Vec::new(),
        },
    ))
    .with_valid_time("2026-01-02T12:00:00Z", "log_event_timestamp");
    let aggregates = GraphRecord::edge(
        EdgeLabel::Aggregates,
        bucket_id.clone(),
        sig_new_id.clone(),
        None,
        "LogOccurrenceBucket aggregates ErrorSignature".to_owned(),
    );
    // A resolved backtrace frame onto the seeded `foo` symbol (#322): the frame
    // row is audited under the code-handle rule and is cited by foo's file/span.
    let frame_edge = GraphRecord::Edge {
        id: log_stable_id(&[
            "edge",
            "FRAME_RESOLVES_TO",
            &sig_new_id,
            "0",
            &symbol_id,
            "resolved",
        ]),
        schema_version: LOG_SCHEMA_VERSION,
        label: EdgeLabel::FrameResolvesTo,
        source: sig_new_id.clone(),
        target: symbol_id.clone(),
        confidence: Some("1.0".to_owned()),
        resolution: None,
        frame_resolution: Some(FrameResolution::Resolved),
        frame_index: Some(0),
        basis: None,
        is_exhaustive: None,
        temporal: None,
        summary: format!("frame 0 of {sig_new_id} resolves to {symbol_id}"),
        producer: None,
    };

    // ── Assemble ────────────────────────────────────────────────────────────
    for record in [
        agent,
        session,
        file,
        symbol,
        defines,
        drift,
        drifts_from,
        task,
        ac,
        ver,
        cmd,
        patch,
        claim,
        contra,
        decision,
        authored_by,
        session_of,
        contradicts,
        supersedes,
        candidate,
        prompt,
        udecision,
        pref,
        pending_candidate,
        repo,
        c1,
        c2,
        c3,
        log_source,
        sig_new,
        sig_cont,
        captured_new,
        captured_cont,
        bucket,
        aggregates,
        frame_edge,
    ] {
        graph.push(record);
    }
    for record in promo_obs {
        graph.push(record);
    }
    for record in dependency_records {
        graph.push(record);
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    Fixture {
        _temp: temp,
        graph: path,
        protected_handle,
    }
}

fn run_audit(fixture: &Fixture) -> (Value, Vec<u8>) {
    let output = egregore()
        .args(["audit", "citations", "--graph"])
        .arg(&fixture.graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("valid JSON report");
    (value, output)
}

// AC1/AC2/AC4/AC5: the seeded audit passes the gate and exits 0.
#[test]
fn seeded_audit_passes_gate_exit_zero() {
    let fixture = seed();
    let (report, _) = run_audit(&fixture);
    assert_eq!(report["ok"], Value::Bool(true), "report: {report:#}");
    assert_eq!(report["gate"]["code_gate_pass"], Value::Bool(true));
    assert_eq!(
        report["gate"]["non_code_handle_gate_pass"],
        Value::Bool(true)
    );
    assert_eq!(report["gate"]["unclassified_missing_rows"], Value::from(0));
    assert!(
        report["gate"]["code_citation_completeness"]
            .as_f64()
            .unwrap()
            >= 0.95
    );
}

// AC3: per-workflow and overall counts carry all six required tallies.
#[test]
fn report_has_per_workflow_and_overall_counts() {
    let fixture = seed();
    let (report, _) = run_audit(&fixture);
    let required = [
        "total_rows",
        "rows_with_record_id",
        "rows_with_primary_handle",
        "rows_using_absent_handle_rule",
        "rows_missing_required_handle",
        "rows_excluded_unverified_or_protected",
    ];
    for key in required {
        assert!(report["overall"][key].is_number(), "overall.{key} missing");
    }
    let workflows = report["workflows"].as_array().expect("workflows array");
    for workflow in workflows {
        for key in required {
            assert!(
                workflow["counts"][key].is_number(),
                "{}.counts.{key} missing",
                workflow["workflow"]
            );
        }
    }
}

// AC2: the audit covers symbol, file, drift, semantic, and the cross-domain set.
#[test]
fn covers_all_public_query_workflows() {
    let fixture = seed();
    let (report, _) = run_audit(&fixture);
    let names: Vec<&str> = report["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["workflow"].as_str().unwrap())
        .collect();
    for expected in [
        "symbol",
        "file",
        "drift",
        "semantic",
        "manifest-deps",
        "context",
        "subsystem",
        "task",
        "memory",
        "failures",
        "change-impact",
        "policy",
        "candidates",
        "changes",
        "evidence-freshness",
        "log-deltas",
        // #376: the two remaining log query workflows are now covered.
        "error-context",
        "log_signatures",
    ] {
        assert!(names.contains(&expected), "workflow {expected} missing");
    }
    // The four code-oriented workflows and the user-context policy lane all
    // returned at least one classified row.
    let rows_for = |name: &str| -> u64 {
        report["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["workflow"] == name)
            .unwrap()["counts"]["total_rows"]
            .as_u64()
            .unwrap()
    };
    assert!(rows_for("symbol") >= 1);
    assert!(rows_for("file") >= 1);
    assert!(rows_for("drift") >= 1);
    // PR #314 review: the manifest-deps lane is seeded and gated too.
    assert!(
        rows_for("manifest-deps") >= 2,
        "manifest-deps lane should surface the seeded dependency declarations"
    );
    assert!(
        rows_for("policy") >= 1,
        "policy lane should surface the durable preference"
    );
    // Review #1: a Task carrying an entity_id must still be audited (the anchor
    // is its record ID, not the entity_id), so the task lane returns rows.
    assert!(
        rows_for("task") >= 1,
        "task lane should surface the entity-id task and its evidence"
    );
    // Review #4: pending candidates and observation freshness are now gated.
    assert!(
        rows_for("candidates") >= 1,
        "candidates lane should surface the pending PromoteCandidate"
    );
    assert!(
        rows_for("evidence-freshness") >= 1,
        "evidence-freshness lane should surface observation verdicts"
    );
    // #328: the log-deltas lane surfaces the seeded runtime-observation
    // signatures (new + continuing) plus the resolved-frame code row.
    assert!(
        rows_for("log-deltas") >= 2,
        "log-deltas lane should surface the seeded runtime-observation signatures"
    );
    // #376: the error-context lane resolves every seeded ErrorSignature and the
    // log_signatures lane surfaces each signature whose frame resolves under a
    // subsystem prefix — both are runtime-observation log query workflows.
    assert!(
        rows_for("error-context") >= 2,
        "error-context lane should surface both seeded runtime-observation signatures"
    );
    assert!(
        rows_for("log_signatures") >= 1,
        "log_signatures lane should surface the frame-resolved runtime-observation signature"
    );
}

// AC2/AC3: semantic is reported with a stable disabled reason over --graph.
#[test]
fn semantic_disabled_over_graph() {
    let fixture = seed();
    let (report, _) = run_audit(&fixture);
    let semantic = report["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workflow"] == "semantic")
        .unwrap();
    assert_eq!(semantic["enabled"], Value::Bool(false));
    assert_eq!(semantic["disabled_reason"], "requires_embedded_store");
}

// AC9: repeating the same seeded audit is byte-identical across 5 runs.
#[test]
fn repeating_audit_5x_is_byte_identical() {
    let fixture = seed();
    let (_, first) = run_audit(&fixture);
    for _ in 0..4 {
        let (_, again) = run_audit(&fixture);
        assert_eq!(first, again, "audit output must be deterministic");
    }
}

// AC8: output never contains any raw payload sentinel or protected raw bytes.
#[test]
fn output_never_contains_raw_payloads() {
    let fixture = seed();
    let (_, stdout) = run_audit(&fixture);
    let text = String::from_utf8(stdout).expect("utf-8 output");
    for sentinel in SENTINELS {
        assert!(
            !text.contains(sentinel),
            "output leaked sentinel {sentinel}"
        );
    }
    // The protected handle (not its bytes) is allowed and present.
    assert!(text.contains(&fixture.protected_handle));
}

// AC4: the gate fails (exit 1) when a code-answer row lacks a file/span handle.
#[test]
fn gate_fails_when_code_citation_below_threshold() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("code_miss.jsonl");
    let mut graph = Graph::new();
    // One cited symbol and one span-less, path-less symbol → 50% completeness.
    graph.push(GraphRecord::syntax_node(
        stable_id(&["node", "Symbol", "src/a.rs", "alpha"]),
        NodeKind::Symbol,
        "src/a.rs".to_owned(),
        span(1, 5),
        "alpha".to_owned(),
        "rust",
        "alpha".to_owned(),
    ));
    graph.push(GraphRecord::node(
        stable_id(&["node", "Symbol", "beta"]),
        NodeKind::Symbol,
        None,
        None,
        Some("beta".to_owned()),
        "beta".to_owned(),
    ));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    egregore()
        .args(["audit", "citations", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .code(1);
}

// AC5: the gate fails (exit 1) when an agent-memory row lacks a source handle.
#[test]
fn gate_fails_when_noncode_row_missing_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("noncode_miss.jsonl");
    let mut graph = Graph::new();
    let mut obs = GraphRecord::node(
        agent_memory_stable_id(&["obs", "bare"]),
        NodeKind::Observation,
        None,
        None,
        None,
        "bare observation".to_owned(),
    );
    // Genuinely provenance-less: no source handle, no evidence link, and no
    // agent/session provenance handle — the row carries nothing citable.
    if let GraphRecord::Node { schema_version, .. } = &mut obs {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    graph.push(obs);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    egregore()
        .args(["audit", "citations", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .code(1);
}

// #328: the green-path (all-cited) log seed passes at the strictest default
// gate — every runtime_observation signature row is cited by its LogSource
// provenance, and the log lane meets `--min-log-citation 1.0`.
#[test]
fn log_lane_green_path_all_cited_passes() {
    let fixture = seed();
    let (report, _) = run_audit(&fixture);
    assert_eq!(report["ok"], Value::Bool(true), "report: {report:#}");
    assert_eq!(report["gate"]["log_gate_pass"], Value::Bool(true));
    assert!(
        (report["gate"]["log_citation_completeness"]
            .as_f64()
            .unwrap()
            - 1.0)
            .abs()
            < 1e-9
    );
    assert_eq!(report["min_log_citation"], Value::from(1.0));
    let log = workflow(&report, "log-deltas");
    assert_eq!(log["trust_class"], "runtime_observation");
    // Every runtime_observation signature row is cited by LogSource provenance.
    let sig_rows: Vec<&Value> = log["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["trust_class"] == "runtime_observation")
        .collect();
    assert!(
        !sig_rows.is_empty(),
        "log lane must classify signature rows"
    );
    for row in sig_rows {
        assert_eq!(row["status"], "cited", "row: {row:#}");
        assert!(
            row["primary_handle"]
                .as_str()
                .unwrap()
                .contains("logs/app.log@"),
            "runtime observation must cite its LogSource provenance: {row:#}"
        );
    }
}

// Builds a minimal commit range plus one in-window ErrorSignature with NO
// resolvable CAPTURED_FROM LogSource — a deliberately uncited runtime
// observation for the red-path gate assertion.
fn seed_uncited_log_graph(path: &std::path::Path) {
    let mut graph = Graph::new();
    let commit = |sha: &str, parents: &[&str], valid_time: &str| -> GraphRecord {
        GraphRecord::node(
            stable_id(&["node", "commit", "red-repo", sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: sha.to_owned(),
            git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
            valid_time: valid_time.to_owned(),
            author_time: Some(valid_time.to_owned()),
            observed_at: valid_time.to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        })
    };
    graph.push(commit("c1sha0000", &[], "2026-01-01T00:00:00Z"));
    graph.push(commit("c2sha0000", &["c1sha0000"], "2026-01-02T00:00:00Z"));
    graph.push(commit("c3sha0000", &["c2sha0000"], "2026-01-03T00:00:00Z"));
    // In-window `new` signature, but NO CAPTURED_FROM edge → no LogSource
    // provenance → uncited runtime observation.
    graph.push(
        GraphRecord::node(
            log_stable_id(&[
                "error_signature",
                "red-repo",
                "template-v1",
                "orphan",
                "error",
            ]),
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            "Error signature: orphan".to_owned(),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
            fingerprint_algorithm: "template-v1".to_owned(),
            template_excerpt: "orphan boom".to_owned(),
            severity: "error".to_owned(),
            occurrence_count: 1,
            first_seen: "2026-01-02T12:00:00Z".to_owned(),
            last_seen: "2026-01-02T13:00:00Z".to_owned(),
            frames: None,
            repository_id: "audit-repo".to_owned(),
        }))
        .with_valid_time("2026-01-02T12:00:00Z", "log_event_timestamp"),
    );
    fs::write(path, graph.to_jsonl().expect("serialize")).expect("write");
}

// #328: an uncited runtime-observation row drops the log rate below the gate,
// failing with exit 1 and the `below_log_citation_threshold` diagnostic naming
// the workflow and the runtime_observation class.
#[test]
fn log_lane_red_path_uncited_fails_gate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("uncited_log.jsonl");
    seed_uncited_log_graph(&path);

    let output = egregore()
        .args(["audit", "citations", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).expect("valid JSON report");
    assert_eq!(report["ok"], Value::Bool(false));
    assert_eq!(report["gate"]["log_gate_pass"], Value::Bool(false));
    assert!(
        report["gate"]["log_citation_completeness"]
            .as_f64()
            .unwrap()
            < 1.0
    );
    let diags = report["diagnostics"].as_array().unwrap();
    // #376: the diagnostic names the SPECIFIC failing workflow. The uncited
    // orphan signature surfaces through both the `log-deltas` and `error-context`
    // log query lanes, so each emits its own below-threshold diagnostic naming
    // itself — never a single hard-coded `log-deltas`.
    let below: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == "below_log_citation_threshold")
        .collect();
    assert!(
        !below.is_empty(),
        "below_log_citation_threshold diagnostic must be present"
    );
    for d in &below {
        assert_eq!(d["relation"], "runtime_observation");
    }
    let named: Vec<&str> = below
        .iter()
        .map(|d| d["workflow"].as_str().unwrap())
        .collect();
    assert!(
        named.contains(&"log-deltas"),
        "the log-deltas lane must name itself: {named:?}"
    );
    assert!(
        named.contains(&"error-context"),
        "the error-context lane must name itself: {named:?}"
    );
}

// #328: an out-of-range --min-log-citation is a usage error (exit 2), mirroring
// --min-code-citation.
#[test]
fn invalid_min_log_citation_is_rejected() {
    let fixture = seed();
    for bad in ["-1", "2", "nan"] {
        egregore()
            .args(["audit", "citations", "--graph"])
            .arg(&fixture.graph)
            .args(["--min-log-citation", bad])
            .assert()
            .failure()
            .code(2);
    }
}

// Success metric: the seeded audit completes well under 2 seconds.
#[test]
fn completes_under_two_seconds() {
    let fixture = seed();
    let start = Instant::now();
    let _ = run_audit(&fixture);
    assert!(
        start.elapsed().as_secs() < 2,
        "audit exceeded the 2s budget"
    );
}

// AC1: a bad path is a usage error (exit 2), distinct from a gate failure.
#[test]
fn missing_graph_is_usage_error_exit_two() {
    egregore()
        .args(["audit", "citations", "--graph", "/nonexistent/path.jsonl"])
        .assert()
        .failure()
        .code(2);
}

// AC10: the documentation shows the shortest workflow, explains pass/fail, and
// states when to use this versus the boring substitutes.
#[test]
fn documentation_covers_workflow_passfail_and_alternatives() {
    let doc = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/cli/citation-audit.md"
    ))
    .expect("citation-audit.md should exist");
    assert!(
        doc.contains("eg audit citations --graph"),
        "missing shortest workflow"
    );
    assert!(doc.contains("Shortest local workflow"));
    assert!(doc.contains("interpret pass/fail") || doc.contains("How to interpret pass/fail"));
    assert!(doc.contains("code_gate_pass") && doc.contains("non_code_handle_gate_pass"));
    for tool in [
        "rg",
        "jq",
        "GitHub Code Search",
        "Sourcegraph",
        "transcript",
    ] {
        assert!(doc.contains(tool), "doc should compare against {tool}");
    }
}

// AC2 (user decision #2): over an embedded store with embeddings, the
// `semantic` workflow is actually driven and returns classified code rows.
//
// Ignored by default: `--embed` loads the re-exported embedding model, which is
// fetched on first use. Egregore never *requires* remote embeddings (issue #65
// AC1), so the offline test matrix must not depend on the model being present.
// Run explicitly where the model is available locally:
//   cargo test --features embeddings --test citation_audit -- --ignored
#[cfg(feature = "embeddings")]
#[ignore = "requires a locally available embedding model (offline-incompatible)"]
#[test]
fn semantic_workflow_enabled_over_embedded_store() {
    use aletheia_egregore::ir::SCHEMA_VERSION;

    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("semantic.graph.jsonl");
    let data_dir = temp.path().join("semantic-store");

    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "audit-repo"]),
        NodeKind::Repository,
        None,
        None,
        Some("audit-repo".to_owned()),
        "Repository audit-repo".to_owned(),
    ));
    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    graph.push(GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    ));
    let symbol_id = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
    graph.push(GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        "src/lib.rs".to_owned(),
        span(10, 20),
        "foo".to_owned(),
        "rust",
        "Symbol foo parses configuration input".to_owned(),
    ));
    let _ = SCHEMA_VERSION;
    fs::write(&graph_path, graph.to_jsonl().expect("serialize")).expect("write");

    // Build an embedded store with local embeddings (no network beyond the
    // re-exported model boundary; no remote indexing).
    egregore()
        .args(["ingest"])
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .success();

    let output = egregore()
        .args(["audit", "citations", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).expect("valid JSON report");

    let semantic = report["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workflow"] == "semantic")
        .expect("semantic workflow present");
    assert_eq!(
        semantic["enabled"],
        Value::Bool(true),
        "semantic should be enabled over an embedded store: {report:#}"
    );
    assert!(
        semantic["counts"]["total_rows"].as_u64().unwrap() >= 1,
        "semantic should return at least one classified code row"
    );
    assert_eq!(report["ok"], Value::Bool(true));
}

// ---------------------------------------------------------------------------
// Regression tests for PR #250 Codex review fixes
// ---------------------------------------------------------------------------

/// Runs the audit without asserting an exit code; returns the parsed report and
/// whether the gate passed (exit 0).
fn audit_report(path: &std::path::Path) -> (Value, bool) {
    let output = egregore()
        .args(["audit", "citations", "--graph"])
        .arg(path)
        .output()
        .expect("run audit");
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    (report, output.status.success())
}

fn workflow<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workflow"] == name)
        .unwrap_or_else(|| panic!("workflow {name} missing"))
}

fn temporal(commit: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

// Review #2: a subsystem's semantic_drift row is classified by its resolved
// target handle (trust class `source_fact`), not credited generically by the
// drift node's own ID (`other`).
#[test]
fn subsystem_drift_classified_by_target_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("subsystem_drift.jsonl");
    let mut graph = Graph::new();

    let file_id = stable_id(&["node", "File", "src/sub/lib.rs"]);
    graph.push(GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/sub/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "file".to_owned(),
    ));
    let symbol_id = stable_id(&["node", "Symbol", "src/sub/lib.rs", "bar"]);
    graph.push(GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        "src/sub/lib.rs".to_owned(),
        span(10, 20),
        "bar".to_owned(),
        "rust",
        "symbol bar".to_owned(),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        symbol_id.clone(),
        None,
        "defines bar".to_owned(),
    ));

    let drift_id = semantic_stable_id(&["drift", "bar"]);
    let mut drift = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "drift on bar".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        semantic_drift,
        ..
    } = &mut drift
    {
        *schema_version = SEMANTIC_SCHEMA_VERSION;
        *domain = Some("semantic".to_owned());
        *semantic_drift = Some(Box::new(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "aletheiadb_re_export".to_owned(),
                name: "m".to_owned(),
                version: "0.1.0".to_owned(),
                dim: 384,
                content_hash: "unknown".to_owned(),
            },
            target_record_id: symbol_id.clone(),
            prior_record_id: symbol_id.clone(),
            before_git_commit: "aaaa".to_owned(),
            after_git_commit: "bbbb".to_owned(),
            before_valid_time: "2026-06-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-06-02T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.8,
            selection_threshold: 0.4,
            selection_basis: SelectionBasis::ThresholdOnly,
        }));
    }
    graph.push(drift);
    graph.push(GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.clone(),
        symbol_id,
        None,
        "drifts from bar".to_owned(),
    ));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, ok) = audit_report(&path);
    assert!(ok, "gate should pass: {report:#}");
    let subsystem = workflow(&report, "subsystem");
    let drift_row = subsystem["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["record_id"] == drift_id)
        .expect("subsystem should classify the drift row");
    assert_eq!(
        drift_row["trust_class"], "source_fact",
        "drift row must be classified by its target handle, not as `other`"
    );
    assert_eq!(
        drift_row["primary_handle"], "src/sub/lib.rs:10-20",
        "drift row should carry its resolved target file/span"
    );
}

// Issue #108 code review: `eg query context`'s new drift_history section must
// be visible to the citation audit too, classified by its resolved target
// handle exactly like `drive_subsystem`'s semantic_drift rows.
#[test]
fn context_drift_classified_by_target_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("context_drift.jsonl");
    let mut graph = Graph::new();

    let file_id = stable_id(&["node", "File", "src/ctx/lib.rs"]);
    graph.push(GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/ctx/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "file".to_owned(),
    ));
    let symbol_id = stable_id(&["node", "Symbol", "src/ctx/lib.rs", "baz"]);
    graph.push(GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        "src/ctx/lib.rs".to_owned(),
        span(10, 20),
        "baz".to_owned(),
        "rust",
        "symbol baz".to_owned(),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        symbol_id.clone(),
        None,
        "defines baz".to_owned(),
    ));

    let drift_id = semantic_stable_id(&["drift", "baz"]);
    let mut drift = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "drift on baz".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        semantic_drift,
        ..
    } = &mut drift
    {
        *schema_version = SEMANTIC_SCHEMA_VERSION;
        *domain = Some("semantic".to_owned());
        *semantic_drift = Some(Box::new(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "aletheiadb_re_export".to_owned(),
                name: "m".to_owned(),
                version: "0.1.0".to_owned(),
                dim: 384,
                content_hash: "unknown".to_owned(),
            },
            target_record_id: symbol_id.clone(),
            prior_record_id: symbol_id.clone(),
            before_git_commit: "aaaa".to_owned(),
            after_git_commit: "bbbb".to_owned(),
            before_valid_time: "2026-06-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-06-02T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.8,
            selection_threshold: 0.4,
            selection_basis: SelectionBasis::ThresholdOnly,
        }));
    }
    graph.push(drift);
    graph.push(GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.clone(),
        symbol_id,
        None,
        "drifts from baz".to_owned(),
    ));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, ok) = audit_report(&path);
    assert!(ok, "gate should pass: {report:#}");
    let context = workflow(&report, "context");
    let drift_row = context["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["record_id"] == drift_id)
        .expect("context workflow should classify the drift row");
    assert_eq!(
        drift_row["trust_class"], "source_fact",
        "drift row must be classified by its target handle, not as `other`"
    );
    assert_eq!(
        drift_row["primary_handle"], "src/ctx/lib.rs:10-20",
        "drift row should carry its resolved target file/span"
    );
}

// Review #3: scan-history graphs carry multiple temporal versions sharing one
// stable record_id; a later span-less version must be counted, not hidden by an
// earlier cited version, so the gate fails.
#[test]
fn temporal_versions_each_counted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("temporal.jsonl");
    let mut graph = Graph::new();

    // Both versions share the same identity-derived record_id (ADR-0004).
    let symbol_id = stable_id(&["node", "Symbol", "src/lib.rs", "foo"]);
    // Earlier version: cited (path + span).
    graph.push(
        GraphRecord::syntax_node(
            symbol_id.clone(),
            NodeKind::Symbol,
            "src/lib.rs".to_owned(),
            span(10, 20),
            "foo".to_owned(),
            "rust",
            "foo @ aaaa".to_owned(),
        )
        .with_temporal(temporal("aaaaaaaa")),
    );
    // Later version: span-less → a real uncited code-answer row.
    let mut later = GraphRecord::node(
        symbol_id.clone(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("foo".to_owned()),
        "foo @ bbbb".to_owned(),
    );
    later = later.with_temporal(temporal("bbbbbbbb"));
    graph.push(later);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, ok) = audit_report(&path);
    assert!(
        !ok,
        "later span-less version must fail the gate: {report:#}"
    );
    let symbol = workflow(&report, "symbol");
    assert_eq!(
        symbol["counts"]["total_rows"].as_u64().unwrap(),
        2,
        "both temporal versions must be counted, not collapsed by record_id"
    );
}

// Review #4: a symbol name that is ambiguous across repositories is driven for
// each candidate record ID (with an `ambiguous_code_handle` diagnostic), not
// dropped — so rows behind the ambiguous handle are still measured.
#[test]
fn ambiguous_multi_repo_anchor_is_driven() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ambiguous.jsonl");
    let mut graph = Graph::new();

    for repo in ["repo-a", "repo-b"] {
        let repo_id = stable_id(&["node", "Repository", repo]);
        graph.push(GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(repo.to_owned()),
            format!("Repository {repo}"),
        ));
        let fpath = format!("src/{repo}_mod.rs");
        let f_id = stable_id(&["node", "File", &fpath]);
        graph.push(GraphRecord::syntax_node(
            f_id.clone(),
            NodeKind::File,
            fpath.clone(),
            span(1, 50),
            "mod.rs".to_owned(),
            "rust",
            format!("file {fpath}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id,
            f_id.clone(),
            None,
            "repo contains file".to_owned(),
        ));
        let w_id = stable_id(&["node", "Symbol", &fpath, "Widget"]);
        graph.push(GraphRecord::syntax_node(
            w_id.clone(),
            NodeKind::Symbol,
            fpath.clone(),
            span(5, 10),
            "Widget".to_owned(),
            "rust",
            "struct Widget".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            f_id,
            w_id,
            None,
            "defines Widget".to_owned(),
        ));
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, ok) = audit_report(&path);
    assert!(ok, "gate should pass: {report:#}");
    // The ambiguous handle is recorded as a diagnostic, not silently dropped.
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "ambiguous_code_handle"),
        "expected an ambiguous_code_handle diagnostic: {report:#}"
    );
    // Both repositories' Widget files surface as change-impact leads (containing
    // context), proving each ambiguous candidate was driven rather than skipped.
    assert!(
        workflow(&report, "change-impact")["counts"]["total_rows"]
            .as_u64()
            .unwrap()
            >= 2,
        "both ambiguous candidates should be driven: {report:#}"
    );
}

// Review #5: the `changes` workflow audits a commit range derived from the
// record set, and reports a stable disabled reason when there is no range.
#[test]
fn changes_workflow_audits_commit_range() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("changes.jsonl");
    let mut graph = Graph::new();

    let commit = |sha: &str, parents: &[&str]| -> GraphRecord {
        GraphRecord::node(
            stable_id(&["node", "commit", sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: sha.to_owned(),
            git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        })
    };
    graph.push(commit("aaaaaaaa", &[]));
    graph.push(commit("bbbbbbbb", &["aaaaaaaa"]));
    graph.push(GraphRecord::edge(
        EdgeLabel::ParentOf,
        stable_id(&["node", "commit", "aaaaaaaa"]),
        stable_id(&["node", "commit", "bbbbbbbb"]),
        Some("1.0".to_owned()),
        "aaaa parent of bbbb".to_owned(),
    ));
    // A file changed in the head commit (cited: path + span).
    let f_id = stable_id(&["node", "File", "src/lib.rs"]);
    graph.push(
        GraphRecord::syntax_node(
            f_id.clone(),
            NodeKind::File,
            "src/lib.rs".to_owned(),
            span(1, 40),
            "lib.rs".to_owned(),
            "rust",
            "file".to_owned(),
        )
        .with_temporal(temporal("bbbbbbbb")),
    );
    graph.push(GraphRecord::edge(
        EdgeLabel::ChangedIn,
        f_id,
        stable_id(&["node", "commit", "bbbbbbbb"]),
        None,
        "lib.rs changed in bbbb".to_owned(),
    ));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, _ok) = audit_report(&path);
    let changes = workflow(&report, "changes");
    assert_eq!(
        changes["enabled"],
        Value::Bool(true),
        "changes: {changes:#}"
    );
    assert!(
        changes["counts"]["total_rows"].as_u64().unwrap() >= 1,
        "changes should audit the commit-range rows: {changes:#}"
    );
}

// Review #5: with no commit range, `changes` is reported disabled, not skipped.
// (The main `seed()` now carries a commit range for the #328 log-deltas lane, so
// this uses a dedicated commit-less graph — a single cited symbol.)
#[test]
fn changes_disabled_without_commit_range() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("no_commits.jsonl");
    let mut graph = Graph::new();
    graph.push(GraphRecord::syntax_node(
        stable_id(&["node", "Symbol", "src/a.rs", "alpha"]),
        NodeKind::Symbol,
        "src/a.rs".to_owned(),
        span(1, 5),
        "alpha".to_owned(),
        "rust",
        "alpha".to_owned(),
    ));
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, _ok) = audit_report(&path);
    let changes = workflow(&report, "changes");
    assert_eq!(changes["enabled"], Value::Bool(false));
    assert_eq!(changes["disabled_reason"], "requires_commit_range");
    // The log-deltas lane is likewise disabled without a commit range.
    let log = workflow(&report, "log-deltas");
    assert_eq!(log["enabled"], Value::Bool(false));
    assert_eq!(log["disabled_reason"], "requires_commit_range");
}

// Review #1: a scan-history symbol that was later deleted still has temporal
// rows in `eg query symbol`, so the audit must count them rather than dropping
// every symbol whose stable ID appears in a tombstone.
#[test]
fn historical_tombstoned_symbol_is_counted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("hist_tombstone.jsonl");
    let mut graph = Graph::new();

    let symbol_id = stable_id(&["node", "Symbol", "src/lib.rs", "gone"]);
    graph.push(
        GraphRecord::syntax_node(
            symbol_id.clone(),
            NodeKind::Symbol,
            "src/lib.rs".to_owned(),
            span(10, 20),
            "gone".to_owned(),
            "rust",
            "gone @ aaaa".to_owned(),
        )
        .with_temporal(TemporalMetadata {
            git_commit: "aaaaaaaa".to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        }),
    );
    // The symbol was deleted in a later commit.
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tombstone", &symbol_id]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: symbol_id,
        summary: "deleted gone".to_owned(),
        producer: None,
    });
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let (report, _ok) = audit_report(&path);
    assert!(
        workflow(&report, "symbol")["counts"]["total_rows"]
            .as_u64()
            .unwrap()
            >= 1,
        "historical version of a tombstoned symbol must be counted: {report:#}"
    );
}

// Review round 5: an out-of-range --min-code-citation cannot silently disable
// the gate; it is rejected as a usage error.
#[test]
fn invalid_min_code_citation_is_rejected() {
    let fixture = seed();
    for bad in ["-1", "2", "nan"] {
        egregore()
            .args(["audit", "citations", "--graph"])
            .arg(&fixture.graph)
            .args(["--min-code-citation", bad])
            .assert()
            .failure()
            .code(2);
    }
}
