#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::redundant_clone,
    clippy::unnecessary_unwrap
)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, UserContextFields, UserContextScope,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, USER_CONTEXT_SCHEMA_VERSION, agent_memory_stable_id,
        stable_id, user_context_stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

/// Helper to generate a seeded graph JSONL fixture containing:
/// - 3 pending preference candidates
/// - 2 prior decisions
/// - at least 6 supporting evidence links
fn fixture_preference_approval_seeded() -> (tempfile::TempDir, PathBuf, Vec<String>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("preference_approval_seeded.jsonl");

    let mut graph = Graph::new();

    // 1. Core Repository node
    let repo_id = stable_id(&["node", "Repository", "egregore"]);
    let repo = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("egregore".to_owned()),
        "Repository egregore".to_owned(),
    );
    graph.push(repo);

    // 2. Supporting Evidence: 6 observations across 2 sessions (sess_1, sess_2)
    let mut obs_ids = Vec::new();
    for i in 1..=6 {
        let obs_id = agent_memory_stable_id(&["obs", &format!("obs_{i}")]);
        obs_ids.push(obs_id.clone());
        let session_id = if i <= 3 { "sess_1" } else { "sess_2" };
        let mut obs = GraphRecord::node(
            obs_id,
            NodeKind::Observation,
            None,
            None,
            None,
            format!("Supporting observation {i}").to_owned(),
        );
        if let GraphRecord::Node {
            ref mut text,
            ref mut agent_id,
            session_id: ref mut session_id_field,
            ref mut observed_at,
            ref mut confidence,
            ref mut schema_version,
            ref mut domain,
            ..
        } = obs
        {
            *text = Some(format!("Observed pattern {i}").to_owned());
            *agent_id = Some("agent_1".to_owned());
            *session_id_field = Some(session_id.to_owned());
            *observed_at = Some("2026-06-03T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
        }
        graph.push(obs);
    }

    // 3. Three pending candidate promotions:
    // Candidate 1: Rust style preference
    let cand_1_text = "Prefer match over if-let for simple options";
    let scope_1 = UserContextScope {
        repo: Some("egregore".to_owned()),
        path_glob: Some("src/**/*.rs".to_owned()),
        language: Some("rust".to_owned()),
        lifecycle_phase: Some("pre_commit".to_owned()),
    };
    let cand_1_id =
        user_context_stable_id(&["candidate", &blake3::hash(cand_1_text.as_bytes()).to_hex()]);
    let mut cand_1 = GraphRecord::node(
        cand_1_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Rust style promotion candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-03T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(cand_1_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(scope_1),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some(obs_ids[0].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[1].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[3].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            contradicting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }
    graph.push(cand_1.clone());

    // Connect edges for Candidate 1
    for obs_id in &[&obs_ids[0], &obs_ids[1], &obs_ids[3]] {
        graph.push(GraphRecord::edge(
            EdgeLabel::ProposedBy,
            cand_1_id.clone(),
            (*obs_id).clone(),
            Some("0.9".to_owned()),
            "Candidate proposed by Observation".to_owned(),
        ));
    }

    // Candidate 2: Workflow rule
    let cand_2_text = "Always run cargo clippy before committing";
    let scope_2 = UserContextScope {
        repo: Some("egregore".to_owned()),
        path_glob: None,
        language: Some("rust".to_owned()),
        lifecycle_phase: Some("pre_commit".to_owned()),
    };
    let cand_2_id =
        user_context_stable_id(&["candidate", &blake3::hash(cand_2_text.as_bytes()).to_hex()]);
    let mut cand_2 = GraphRecord::node(
        cand_2_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Cargo clippy workflow candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand_2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.85".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-03T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(cand_2_text.to_owned()),
            proposed_rule_kind: Some("workflow_rule".to_owned()),
            scope: Some(scope_2),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some(obs_ids[2].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.85".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[4].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.85".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[5].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.85".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            contradicting_evidence: Some(vec![]),
            triggers: Some(vec!["pre_commit".to_owned()]),
            action_summary: Some("Run clippy validation".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(cand_2.clone());

    for obs_id in &[&obs_ids[2], &obs_ids[4], &obs_ids[5]] {
        graph.push(GraphRecord::edge(
            EdgeLabel::ProposedBy,
            cand_2_id.clone(),
            (*obs_id).clone(),
            Some("0.85".to_owned()),
            "Candidate proposed by Observation".to_owned(),
        ));
    }

    // Candidate 3: Constraint
    let cand_3_text = "No unsafe code blocks allowed";
    let scope_3 = UserContextScope {
        repo: Some("egregore".to_owned()),
        path_glob: Some("src/**/*.rs".to_owned()),
        language: Some("rust".to_owned()),
        lifecycle_phase: Some("any".to_owned()),
    };
    let cand_3_id =
        user_context_stable_id(&["candidate", &blake3::hash(cand_3_text.as_bytes()).to_hex()]);
    let mut cand_3 = GraphRecord::node(
        cand_3_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "No unsafe code constraint candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand_3
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.95".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-03T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(cand_3_text.to_owned()),
            proposed_rule_kind: Some("constraint".to_owned()),
            scope: Some(scope_3),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some(obs_ids[0].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.95".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[2].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.95".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[4].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.95".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            contradicting_evidence: Some(vec![]),
            enforcement_level: Some("blocking".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(cand_3.clone());

    for obs_id in &[&obs_ids[0], &obs_ids[2], &obs_ids[4]] {
        graph.push(GraphRecord::edge(
            EdgeLabel::ProposedBy,
            cand_3_id.clone(),
            (*obs_id).clone(),
            Some("0.95".to_owned()),
            "Candidate proposed by Observation".to_owned(),
        ));
    }

    // 4. Two prior decisions (one approved Preference, one rejected Preference)
    // Seed an approved Preference decision trace
    let old_cand_text = "Prior approved candidate preference";
    let old_cand_id = user_context_stable_id(&[
        "candidate",
        &blake3::hash(old_cand_text.as_bytes()).to_hex(),
    ]);
    let mut old_cand = GraphRecord::node(
        old_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Old approved candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = old_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some(obs_ids[0].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[1].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[3].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            ..UserContextFields::empty()
        };
    }
    graph.push(old_cand);

    let prompt_1_id = user_context_stable_id(&["prompt", "prompt_old_approved"]);
    let mut prompt_1 = GraphRecord::node(
        prompt_1_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Old approved prompt".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = prompt_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("Do you approve this prior preference?".to_owned()),
            prompted_at: Some("2026-06-01T10:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(prompt_1);
    graph.push(GraphRecord::edge(
        EdgeLabel::PromptedFor,
        prompt_1_id.clone(),
        old_cand_id.clone(),
        None,
        "Prompted for Candidate".to_owned(),
    ));

    let decision_1_id = user_context_stable_id(&["decision", "decision_old_approved"]);
    let pref_1_id = user_context_stable_id(&["preference", "pref_old_approved"]);

    let mut decision_1 = GraphRecord::node(
        decision_1_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Old approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            prompt_id: Some(prompt_1_id),
            outcome: Some("approved".to_owned()),
            decided_at: Some("2026-06-01T10:05:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            materialized_record_id: Some(pref_1_id.clone()),
            ..UserContextFields::empty()
        };
    }
    graph.push(decision_1);
    graph.push(GraphRecord::edge(
        EdgeLabel::DecidedOn,
        decision_1_id.clone(),
        old_cand_id,
        None,
        "Decided on Candidate".to_owned(),
    ));

    let mut pref_1 = GraphRecord::node(
        pref_1_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Old approved preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(decision_1_id.clone()),
            active_from: Some("2026-06-01T10:05:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(pref_1);
    graph.push(GraphRecord::edge(
        EdgeLabel::MaterializedAs,
        decision_1_id,
        pref_1_id,
        None,
        "Materialized as Preference".to_owned(),
    ));

    // Seed a rejected Preference decision trace
    let rejected_cand_text = "Prior rejected candidate preference";
    let rejected_cand_id = user_context_stable_id(&[
        "candidate",
        &blake3::hash(rejected_cand_text.as_bytes()).to_hex(),
    ]);
    let mut rejected_cand = GraphRecord::node(
        rejected_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Old rejected candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = rejected_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(rejected_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some(obs_ids[0].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[1].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(obs_ids[3].clone()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "0.9".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            ..UserContextFields::empty()
        };
    }
    graph.push(rejected_cand);

    let prompt_2_id = user_context_stable_id(&["prompt", "prompt_old_rejected"]);
    let mut prompt_2 = GraphRecord::node(
        prompt_2_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Old rejected prompt".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = prompt_2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(rejected_cand_id.clone()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("Do you approve this prior rejected preference?".to_owned()),
            prompted_at: Some("2026-06-02T10:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(prompt_2);
    graph.push(GraphRecord::edge(
        EdgeLabel::PromptedFor,
        prompt_2_id.clone(),
        rejected_cand_id.clone(),
        None,
        "Prompted for Candidate".to_owned(),
    ));

    let decision_2_id = user_context_stable_id(&["decision", "decision_old_rejected"]);
    let mut decision_2 = GraphRecord::node(
        decision_2_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Old rejected decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision_2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(rejected_cand_id.clone()),
            prompt_id: Some(prompt_2_id),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-02T10:05:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
    graph.push(decision_2);
    graph.push(GraphRecord::edge(
        EdgeLabel::DecidedOn,
        decision_2_id,
        rejected_cand_id,
        None,
        "Decided on Candidate".to_owned(),
    ));

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path, vec![cand_1_id, cand_2_id, cand_3_id])
}

#[test]
fn query_pending_candidates_lists_all_unresolved_candidates() {
    let (_temp, graph, cand_ids) = fixture_preference_approval_seeded();

    let output = egregore()
        .args(["query", "candidates", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true);
    let candidates = parsed["candidates"].as_array().expect("candidate array");

    // We expect exactly the 3 pending ones (not the already approved/rejected ones)
    assert_eq!(candidates.len(), 3);

    for id in &cand_ids {
        assert!(candidates.iter().any(|c| c["id"].as_str() == Some(id)));
    }

    // Verify fields shown in candidates representation
    let cand1 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&cand_ids[0]))
        .unwrap();
    assert_eq!(
        cand1["proposed_rule_text"].as_str(),
        Some("Prefer match over if-let for simple options")
    );
    assert_eq!(cand1["proposed_rule_kind"].as_str(), Some("preference"));
    assert_eq!(cand1["confidence"].as_f64(), Some(0.9));
    assert_eq!(cand1["scope"]["repo"].as_str(), Some("egregore"));
    assert!(cand1["supporting_evidence"].as_array().unwrap().len() >= 3);
}

#[test]
fn query_active_policy_returns_only_approved_records() {
    let (_temp, graph, _) = fixture_preference_approval_seeded();

    let output = egregore()
        .args(["query", "policy", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true);
    let policy = parsed["policy"].as_array().expect("policy array");

    // Only 1 approved Preference node exists in active policy
    assert_eq!(policy.len(), 1);
    let entry = &policy[0];
    assert_eq!(entry["kind"].as_str(), Some("Preference"));
    assert_eq!(
        entry["rule_text"].as_str(),
        Some("Prior approved candidate preference")
    );
    assert!(entry["approval_decision_id"].as_str().is_some());
}

#[test]
fn query_active_policy_supports_scope_filtering() {
    let (_temp, graph, _) = fixture_preference_approval_seeded();

    // Query with matching scope parameters
    let output = egregore()
        .args(["query", "policy", "--repo", "egregore", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], true);
    let policy = parsed["policy"].as_array().expect("policy array");
    // Global scope record matches any scope query
    assert_eq!(policy.len(), 1);

    // Query with mismatching scope (e.g. language python vs the global/rust)
    let output_mismatch = egregore()
        .args(["query", "policy", "--language", "python", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout_mismatch = String::from_utf8(output_mismatch).expect("utf8");
    let parsed_mismatch: serde_json::Value =
        serde_json::from_str(stdout_mismatch.trim()).expect("valid JSON");
    assert_eq!(parsed_mismatch["ok"], true);
    // Should still return global policy (scope is completely empty, so it matches any query)
    assert_eq!(parsed_mismatch["policy"].as_array().unwrap().len(), 1);
}

#[test]
fn query_audit_returns_full_approval_chain_to_observations() {
    let (_temp, graph, _) = fixture_preference_approval_seeded();
    let pref_id = user_context_stable_id(&["preference", "pref_old_approved"]);

    let output = egregore()
        .args(["query", "audit", &pref_id, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true);
    let chain = parsed["audit_chain"].as_array().expect("audit chain");

    // Expected chain length: 1 preference + 1 decision + 1 prompt + 1 candidate + 3 observations = 7
    assert_eq!(chain.len(), 7);

    // Verify first element is the preference
    assert_eq!(chain[0]["kind"].as_str(), Some("Preference"));
    assert_eq!(chain[0]["id"].as_str(), Some(pref_id.as_str()));

    // Verify second is decision
    assert_eq!(chain[1]["kind"].as_str(), Some("PromotionDecision"));
    assert_eq!(
        chain[1]["id"].as_str(),
        Some(user_context_stable_id(&["decision", "decision_old_approved"]).as_str())
    );

    // Verify third is prompt
    assert_eq!(chain[2]["kind"].as_str(), Some("PromotionPrompt"));
    assert_eq!(
        chain[2]["id"].as_str(),
        Some(user_context_stable_id(&["prompt", "prompt_old_approved"]).as_str())
    );

    // Verify fourth is candidate
    assert_eq!(chain[3]["kind"].as_str(), Some("PromoteCandidate"));

    // Verify remaining are observations
    for node in &chain[4..] {
        assert_eq!(node["kind"].as_str(), Some("Observation"));
    }
}

#[test]
fn decide_workflow_supports_approve_reject_defer_expire_outcomes() {
    let (temp, graph, cand_ids) = fixture_preference_approval_seeded();
    let cand_1_id = &cand_ids[0]; // proposed Preference

    // Decide Outcome: Approve
    let out_jsonl = temp.path().join("outcome_approved.jsonl");
    egregore()
        .args(["decide", cand_1_id, "--outcome", "approved", "--graph"])
        .arg(&graph)
        .arg("--out")
        .arg(&out_jsonl)
        .assert()
        .success();

    let content = fs::read_to_string(&out_jsonl).expect("read");
    let records: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Expecting Prompt + Prompt Edge + Decision + Decision Edge + Preference + Materialized Edge = 6 records
    assert_eq!(records.len(), 6);

    let decision = records
        .iter()
        .find(|r| r["kind"] == "PromotionDecision")
        .unwrap();
    assert_eq!(decision["outcome"].as_str(), Some("approved"));

    let prompt = records
        .iter()
        .find(|r| r["kind"] == "PromotionPrompt")
        .unwrap();
    assert_eq!(prompt["candidate_id"].as_str(), Some(cand_1_id.as_str()));

    let preference = records.iter().find(|r| r["kind"] == "Preference").unwrap();
    assert_eq!(
        preference["rule_text"].as_str(),
        Some("Prefer match over if-let for simple options")
    );
    assert_eq!(
        preference["approval_decision_id"].as_str(),
        Some(decision["id"].as_str().unwrap())
    );

    // Decide Outcome: Reject (creates no active policy/preference record)
    let out_rejected = temp.path().join("outcome_rejected.jsonl");
    egregore()
        .args(["decide", cand_1_id, "--outcome", "rejected", "--graph"])
        .arg(&graph)
        .arg("--out")
        .arg(&out_rejected)
        .assert()
        .success();

    let content_rej = fs::read_to_string(&out_rejected).expect("read");
    let records_rej: Vec<serde_json::Value> = content_rej
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Prompt + Prompt Edge + Decision + Decision Edge = 4 records, no Preference record
    assert_eq!(records_rej.len(), 4);
    assert!(records_rej.iter().all(|r| r["kind"] != "Preference"));
}

#[test]
fn decide_workflow_supports_edit_then_approve_outcome() {
    let (temp, graph, cand_ids) = fixture_preference_approval_seeded();
    let cand_1_id = &cand_ids[0]; // proposed Preference

    let out_jsonl = temp.path().join("outcome_edited.jsonl");
    egregore()
        .args([
            "decide",
            cand_1_id,
            "--outcome",
            "edited_then_approved",
            "--edited-rule-text",
            "Prefer standard Rust match style",
            "--graph",
        ])
        .arg(&graph)
        .arg("--out")
        .arg(&out_jsonl)
        .assert()
        .success();

    let content = fs::read_to_string(&out_jsonl).expect("read");
    let records: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let decision = records
        .iter()
        .find(|r| r["kind"] == "PromotionDecision")
        .unwrap();
    assert_eq!(decision["outcome"].as_str(), Some("edited_then_approved"));
    assert_eq!(
        decision["edited_rule_text"].as_str(),
        Some("Prefer standard Rust match style")
    );

    let preference = records.iter().find(|r| r["kind"] == "Preference").unwrap();
    assert_eq!(
        preference["rule_text"].as_str(),
        Some("Prefer standard Rust match style")
    );
}

#[test]
fn candidate_suppressed_if_compatible_with_recently_rejected_candidate() {
    // 1. Prior candidate was rejected in the seeded fixture.
    // Let's create a new candidate that is compatible with the rejected one:
    // "Prior rejected candidate preference" -> new candidate "Prior rejected candidate preference" (exact/compatible text)
    let (temp, graph, _) = fixture_preference_approval_seeded();
    let new_cand_text = "Prior rejected candidate preference";
    let new_cand_id = user_context_stable_id(&["candidate", "new_candidate_attempt_123"]);
    let mut new_cand = GraphRecord::node(
        new_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Compatible candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ref mut superseded_by,
        ..
    } = new_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *superseded_by = Some(user_context_stable_id(&[
            "candidate",
            &blake3::hash("Prior rejected candidate preference".as_bytes()).to_hex(),
        ]));
        *user_context = UserContextFields {
            proposed_rule_text: Some(new_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }

    let content = fs::read_to_string(&graph).expect("read");
    let mut parsed_records: Vec<GraphRecord> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    parsed_records.push(new_cand);
    let mut new_graph = Graph::new();
    for r in parsed_records {
        new_graph.push(r);
    }
    let new_graph_path = temp.path().join("debounce_check.jsonl");
    fs::write(&new_graph_path, new_graph.to_jsonl().unwrap()).unwrap();

    // Query pending candidates on the new graph
    let output = egregore()
        .args(["query", "candidates", "--graph"])
        .arg(&new_graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let candidates = parsed["candidates"].as_array().expect("candidates");
    let target = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id))
        .unwrap();

    // Debounce window defaults to T = 30 days and M = 5 observations.
    // The candidate was just decided, has 0 new observations, so it should be suppressed
    assert_eq!(target["suppressed"].as_str(), Some("rejection_debounce"));
}

#[test]
fn deterministic_outputs_across_5_identical_runs() {
    let (_temp, graph, _) = fixture_preference_approval_seeded();

    let mut first_policy = None;
    for _ in 0..5 {
        let output = egregore()
            .args(["query", "policy", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("utf8");
        if first_policy.is_none() {
            first_policy = Some(stdout);
        } else {
            assert_eq!(first_policy.as_ref().unwrap(), &stdout);
        }
    }
}

#[test]
fn candidate_debounce_requires_both_thresholds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("debounce_thresholds.jsonl");

    let old_cand_text = "Debounced preference text";
    let old_cand_id = user_context_stable_id(&["candidate", "old_cand_id"]);
    let mut old_cand = GraphRecord::node(
        old_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Old candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = old_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let decision_id = user_context_stable_id(&["decision", "old_decision"]);
    let mut decision = GraphRecord::node(
        decision_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Old decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-01T12:05:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 1. Scenario: time elapsed > 30 days (e.g. 2026-07-15T12:00:00Z) but ONLY 1 new observation (M = 1, so additional_count = 1 < 5)
    // Both thresholds are not met (count is unmet). So it should still be suppressed!
    let new_cand_id_1 = user_context_stable_id(&["candidate", "new_cand_1"]);
    let mut new_cand_1 = GraphRecord::node(
        new_cand_id_1.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "New candidate 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = new_cand_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-07-15T12:00:00Z".to_owned()); // Time is OK
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            ..UserContextFields::empty()
        };
    }

    // 2. Scenario: time elapsed < 30 days (e.g. 2026-06-05T12:00:00Z) but 7 observations total (6 new, so additional_count = 6 >= 5)
    // Both thresholds are not met (time is unmet). So it should still be suppressed!
    let new_cand_id_2 = user_context_stable_id(&["candidate", "new_cand_2"]);
    let mut new_cand_2 = GraphRecord::node(
        new_cand_id_2.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "New candidate 2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = new_cand_2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-05T12:00:00Z".to_owned()); // Time is not OK
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-3".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-4".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-5".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-6".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-7".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            ..UserContextFields::empty()
        };
    }

    // 3. Scenario: time elapsed > 30 days AND 6 new observations. Both thresholds met! It should NOT be suppressed.
    let new_cand_id_3 = user_context_stable_id(&["candidate", "new_cand_3"]);
    let mut new_cand_3 = GraphRecord::node(
        new_cand_id_3.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "New candidate 3".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = new_cand_3
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-07-15T12:00:00Z".to_owned()); // Time is OK
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-3".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-4".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-5".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-6".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-7".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(old_cand);
    graph.push(decision);
    graph.push(new_cand_1);
    graph.push(new_cand_2);
    graph.push(new_cand_3);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "candidates", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let candidates = parsed["candidates"].as_array().unwrap();

    let c1 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id_1))
        .unwrap();
    let c2 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id_2))
        .unwrap();
    let c3 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id_3))
        .unwrap();

    assert_eq!(c1["suppressed"].as_str(), Some("rejection_debounce"));
    assert_eq!(c2["suppressed"].as_str(), Some("rejection_debounce"));
    assert!(c3["suppressed"].is_null());
}

#[test]
fn query_candidates_resolves_multiple_decisions_by_timestamp() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("multiple_decisions.jsonl");

    let cand_text = "Preference with multiple decisions";
    let cand_id = user_context_stable_id(&["candidate", "cand_mult"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    // First decision: deferred at 2026-06-01T12:00:00Z
    let d1_id = user_context_stable_id(&["decision", "d1"]);
    let mut d1 = GraphRecord::node(
        d1_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "D1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = d1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.clone()),
            outcome: Some("deferred".to_owned()),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // Second decision: approved at 2026-06-02T12:00:00Z (terminal)
    let d2_id = user_context_stable_id(&["decision", "d2"]);
    let mut d2 = GraphRecord::node(
        d2_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "D2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = d2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.clone()),
            outcome: Some("approved".to_owned()),
            decided_at: Some("2026-06-02T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand.clone());
    graph.push(d1);
    graph.push(d2);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "candidates", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let candidates = parsed["candidates"].as_array().unwrap();

    // Since the latest decision is terminal ("approved"), the candidate should not be pending.
    assert!(
        candidates
            .iter()
            .all(|c| c["id"].as_str() != Some(cand_id.as_str()))
    );
}

#[test]
fn active_policy_excludes_unapproved_durable_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("unapproved_durable.jsonl");

    // 1. Unapproved Preference record (no approval_decision_id)
    let pref_unapproved_id = user_context_stable_id(&["preference", "pref_unapproved"]);
    let mut pref_unapproved = GraphRecord::node(
        pref_unapproved_id,
        NodeKind::Preference,
        None,
        None,
        None,
        "Unapproved policy".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref_unapproved
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Unapproved rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 2. Preference record pointing to a missing or non-approving decision (outcome: "rejected" or "deferred")
    let pref_deferred_id = user_context_stable_id(&["preference", "pref_deferred"]);
    let mut pref_deferred = GraphRecord::node(
        pref_deferred_id,
        NodeKind::Preference,
        None,
        None,
        None,
        "Deferred policy".to_owned(),
    );
    let dec_deferred_id = user_context_stable_id(&["decision", "dec_deferred"]);
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref_deferred
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Deferred rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            approval_decision_id: Some(dec_deferred_id.clone()),
            ..UserContextFields::empty()
        };
    }
    let mut dec_deferred = GraphRecord::node(
        dec_deferred_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Deferred decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = dec_deferred
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("deferred".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(pref_unapproved);
    graph.push(pref_deferred);
    graph.push(dec_deferred);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "policy", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let policies = parsed["policy"].as_array().unwrap();

    // Neither of these unapproved/deferred policies should be returned.
    assert!(policies.is_empty());
}

#[test]
fn decide_naming_decision_and_constraint_preserves_proposed_rule_kind_and_applies_edits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("policy_types.jsonl");

    // 1. Naming decision candidate
    let name_cand_id = user_context_stable_id(&["candidate", "naming_cand"]);
    let mut name_cand = GraphRecord::node(
        name_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Naming candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = name_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-05-20T12:00:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("OriginalName".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("type".to_owned()),
            canonical_name: Some("OriginalName".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    // 2. Constraint candidate
    let const_cand_id = user_context_stable_id(&["candidate", "const_cand"]);
    let mut const_cand = GraphRecord::node(
        const_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Constraint candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = const_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-05-20T12:00:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("OriginalConstraint".to_owned()),
            proposed_rule_kind: Some("constraint".to_owned()),
            enforcement_level: Some("blocking".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(name_cand);
    graph.push(const_cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Decide naming candidate with edited_then_approved
    let name_out_path = temp.path().join("name_out.jsonl");
    egregore()
        .args([
            "decide",
            &name_cand_id,
            "--outcome",
            "edited_then_approved",
            "--edited-rule-text",
            "EditedName",
            "--graph",
        ])
        .arg(&graph_path)
        .arg("--out")
        .arg(&name_out_path)
        .assert()
        .success();

    let name_content = fs::read_to_string(&name_out_path).unwrap();
    let name_records: Vec<serde_json::Value> = name_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let naming_policy = name_records
        .iter()
        .find(|r| r["kind"] == "NamingDecision")
        .unwrap();
    // Point 3: proposed_rule_kind should be preserved
    assert_eq!(
        naming_policy["proposed_rule_kind"].as_str(),
        Some("naming_decision")
    );
    // Point 5: canonical_name should be the edited one ("EditedName")
    assert_eq!(naming_policy["canonical_name"].as_str(), Some("EditedName"));

    // Point 2: Timestamp should not be backdated to candidate's 2026-05-20T12:00:00Z.
    let decided_at_str = name_records
        .iter()
        .find(|r| r["kind"] == "PromotionDecision")
        .unwrap()["decided_at"]
        .as_str()
        .unwrap();
    assert!(!decided_at_str.starts_with("2026-05-20"));

    // Decide constraint candidate
    let const_out_path = temp.path().join("const_out.jsonl");
    egregore()
        .args(["decide", &const_cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&const_out_path)
        .assert()
        .success();

    let const_content = fs::read_to_string(&const_out_path).unwrap();
    let const_records: Vec<serde_json::Value> = const_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let const_policy = const_records
        .iter()
        .find(|r| r["kind"] == "Constraint")
        .unwrap();
    // Point 3: proposed_rule_kind should be preserved for Constraint too
    assert_eq!(
        const_policy["proposed_rule_kind"].as_str(),
        Some("constraint")
    );
}

#[test]
fn decide_fails_if_neither_out_nor_data_dir_specified() {
    let (_temp, graph, cand_ids) = fixture_preference_approval_seeded();
    let cand_1_id = &cand_ids[0];

    // Running decide command without --out and without --data-dir should fail
    egregore()
        .args(["decide", cand_1_id, "--outcome", "approved", "--graph"])
        .arg(&graph)
        .assert()
        .failure();
}

#[test]
fn active_policy_collapses_revoked_duplicate_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("revocation_collapse.jsonl");

    let pref_id = user_context_stable_id(&["preference", "pref_collapse"]);
    let dec_id = user_context_stable_id(&["decision", "dec_collapse"]);

    // 1. Initial approved Preference record (active)
    let mut pref_active = GraphRecord::node(
        pref_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Active preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref_active
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Collapse rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            approval_decision_id: Some(dec_id.clone()),
            ..UserContextFields::empty()
        };
    }

    // 2. Later copy of the same Preference record with active_to set (revoked)
    let mut pref_revoked = GraphRecord::node(
        pref_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Revoked preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref_revoked
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Collapse rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            approval_decision_id: Some(dec_id.clone()),
            active_to: Some("2026-06-03T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 3. Approved decision record
    let mut decision = GraphRecord::node(
        dec_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(pref_active);
    graph.push(pref_revoked);
    graph.push(decision);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "policy", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let policies = parsed["policy"].as_array().unwrap();

    // Since one version of the Preference node is revoked, it should not appear in active policy
    assert!(policies.is_empty());
}

#[test]
fn query_policy_broad_scope_match_includes_specific_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("broad_scope.jsonl");

    let pref_id = user_context_stable_id(&["preference", "pref_broad"]);
    let dec_id = user_context_stable_id(&["decision", "dec_broad"]);

    // Preference scoped to repo = egregore, language = rust, path_glob = src/**/*.rs
    let mut pref = GraphRecord::node(
        pref_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Broad preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = pref
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Broad scope rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            approval_decision_id: Some(dec_id.clone()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                language: Some("rust".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                lifecycle_phase: None,
            }),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        dec_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some(pref_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(pref);
    graph.push(decision);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Query with broad filter --repo egregore, omitting language and path
    let output = egregore()
        .args(["query", "policy", "--repo", "egregore", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let policies = parsed["policy"].as_array().unwrap();

    // It should match and return the policy, despite the policy having a specific language/path glob.
    assert_eq!(policies.len(), 1);
    assert_eq!(
        policies[0]["rule_text"].as_str(),
        Some("Broad scope rule text")
    );
}

#[test]
fn candidate_suppressed_based_on_latest_rejection_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("latest_rejection_debounce.jsonl");

    let old_cand_text = "Debounce candidate text";
    let old_cand_id = user_context_stable_id(&["candidate", "old_debounce_cand"]);
    let mut old_cand = GraphRecord::node(
        old_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Old candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = old_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    // 1. First decision: outcome is "deferred" at 2026-06-01T12:05:00Z
    let d1_id = user_context_stable_id(&["decision", "d1_defer"]);
    let mut d1 = GraphRecord::node(
        d1_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Deferred decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = d1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            outcome: Some("deferred".to_owned()),
            decided_at: Some("2026-06-01T12:05:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 2. Second decision: outcome is "rejected" at 2026-06-02T12:05:00Z (LATEST)
    let d2_id = user_context_stable_id(&["decision", "d2_reject"]);
    let mut d2 = GraphRecord::node(
        d2_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Rejected decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = d2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-02T12:05:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 3. New candidate raising the same preference text (within 30 days)
    let new_cand_id = user_context_stable_id(&["candidate", "new_debounce_cand"]);
    let mut new_cand = GraphRecord::node(
        new_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "New candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ..
    } = new_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-10T12:00:00Z".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(old_cand_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some("agent_memory:v1:obs-1".to_owned()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(old_cand);
    graph.push(d1);
    graph.push(d2);
    graph.push(new_cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "candidates", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let candidates = parsed["candidates"].as_array().unwrap();

    let target = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id))
        .unwrap();
    // It should be debounced based on the latest terminal rejection decision (not deferred)
    assert_eq!(target["suppressed"].as_str(), Some("rejection_debounce"));
}

#[test]
fn decide_fails_if_outcome_is_invalid_or_misspelled() {
    let (temp, graph, cand_ids) = fixture_preference_approval_seeded();
    let cand_1_id = &cand_ids[0];
    let out_jsonl = temp.path().join("invalid_outcome.jsonl");

    // Misspelling outcome as 'approve' (should be 'approved')
    egregore()
        .args(["decide", cand_1_id, "--outcome", "approve", "--graph"])
        .arg(&graph)
        .arg("--out")
        .arg(&out_jsonl)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_workflow_rule_candidate_missing_triggers_or_action_summary() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("missing_workflow_fields.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "workflow_missing_fields"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Workflow candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Always run tests".to_owned()),
            proposed_rule_kind: Some("workflow_rule".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("decision_out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_prompt_surface_is_invalid() {
    let (temp, graph, cand_ids) = fixture_preference_approval_seeded();
    let cand_1_id = &cand_ids[0];
    let out_jsonl = temp.path().join("invalid_prompt_surface.jsonl");

    egregore()
        .args([
            "decide",
            cand_1_id,
            "--outcome",
            "approved",
            "--prompt-surface",
            "invalid-surface",
            "--graph",
        ])
        .arg(&graph)
        .arg("--out")
        .arg(&out_jsonl)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_revocation_target_is_not_active_policy() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("invalid_revocation.jsonl");

    // 1. A PromoteCandidate for revocation targeting an observation (non-policy)
    let obs_id = agent_memory_stable_id(&["obs", "obs_target"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation text".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
    }

    let cand_id = user_context_stable_id(&["candidate", "revocation_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Revocation candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_kind: Some("revocation".to_owned()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(obs_id.clone()),
                target_domain: "user_context".to_owned(),
                relation: "REVOKES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(obs);
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_naming_decision_preserves_edited_canonical_name_hash() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("naming_edit.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "naming_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Naming candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("old_name".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("function".to_owned()),
            canonical_name: Some("old_name".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "edited_then_approved",
            "--edited-rule-text",
            "new_name",
            "--graph",
        ])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let output_jsonl = fs::read_to_string(&out_path).unwrap();
    let records: Vec<GraphRecord> = output_jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL line"))
        .collect();

    let naming_node = records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::NamingDecision,
                    ..
                }
            )
        })
        .unwrap();

    let decision_node = records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    ..
                }
            )
        })
        .unwrap();

    if let (
        GraphRecord::Node {
            id: naming_id,
            user_context: naming_fields,
            ..
        },
        GraphRecord::Node {
            id: decision_id, ..
        },
    ) = (naming_node, decision_node)
    {
        assert_eq!(naming_fields.canonical_name.as_deref(), Some("new_name"));

        // Replicate stable ID hash computation for NamingDecision:
        // blake3_hash_parts(&["function", "new_name", "", &decision_id])
        let mut hasher = blake3::Hasher::new();
        for part in &[
            b"function" as &[u8],
            b"new_name",
            b"",
            decision_id.as_bytes(),
        ] {
            hasher.update(part);
            hasher.update(b"\0");
        }
        let expected_hash = hasher.finalize().to_hex();
        let expected_naming_id =
            user_context_stable_id(&["naming_decision", expected_hash.as_str()]);

        assert_eq!(naming_id, &expected_naming_id);
    } else {
        panic!("Missing expected nodes");
    }
}

#[test]
fn decide_fails_if_naming_decision_missing_required_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("naming_missing.jsonl");

    // Missing entity_kind
    let cand_id = user_context_stable_id(&["candidate", "naming_cand_missing"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Naming candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("name".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            canonical_name: Some("name".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_constraint_missing_enforcement_level() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("constraint_missing.jsonl");

    // Missing enforcement_level
    let cand_id = user_context_stable_id(&["candidate", "constraint_cand_missing"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Constraint candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("No unsafe code".to_owned()),
            proposed_rule_kind: Some("constraint".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_naming_decision_defaults_alternatives_rejected_to_empty_array() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("naming_defaults.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "naming_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Naming candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("name".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("function".to_owned()),
            canonical_name: Some("name".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let output_jsonl = fs::read_to_string(&out_path).unwrap();
    let records: Vec<GraphRecord> = output_jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL line"))
        .collect();

    let naming_node = records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::NamingDecision,
                    ..
                }
            )
        })
        .unwrap();

    if let GraphRecord::Node { user_context, .. } = naming_node {
        assert_eq!(user_context.alternatives_rejected.as_ref(), Some(&vec![]));
    }
}

#[test]
fn active_policy_requires_decision_to_target_the_policy() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("policy_unrelated.jsonl");

    // Seed a policy and a decision that has a different materialized_record_id
    let policy_id = user_context_stable_id(&["preference", "my_policy"]);
    let dec_id = user_context_stable_id(&["decision", "unrelated_dec"]);

    let mut policy = GraphRecord::node(
        policy_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Rule text".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = policy
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        dec_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some("unrelated_policy_id".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(policy);
    graph.push(decision);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args(["query", "policy", "--graph"])
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let policies = parsed["policy"].as_array().unwrap();

    // The policy should be excluded because the decision targets a different materialized ID
    assert!(policies.is_empty());
}

#[test]
fn audit_trail_validates_decision_nodes_properly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("audit_invalid.jsonl");

    let policy_id = user_context_stable_id(&["preference", "my_policy"]);
    let dec_id = user_context_stable_id(&["decision", "dec_id"]);
    let prompt_id = user_context_stable_id(&["prompt", "prompt_id"]);
    let cand_id = user_context_stable_id(&["candidate", "cand_id"]);

    let mut policy = GraphRecord::node(
        policy_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Rule text".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = policy
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.clone()),
            ..UserContextFields::empty()
        };
    }

    // Seed a PromotionDecision node that targets a different materialized ID
    let mut decision = GraphRecord::node(
        dec_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some("unrelated_policy_id".to_owned()),
            prompt_id: Some(prompt_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut prompt = GraphRecord::node(
        prompt_id,
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = prompt
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut candidate = GraphRecord::node(
        cand_id,
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(policy);
    graph.push(decision);
    graph.push(prompt);
    graph.push(candidate);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Querying audit for the policy should fail because the decision targets a different policy record
    egregore()
        .args(["query", "audit", &policy_id, "--graph"])
        .arg(&graph_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_edited_rule_text_is_empty_or_blank() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("empty_edited.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "empty_edited_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Prefer match".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");

    // Test with empty string
    egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "edited_then_approved",
            "--edited-rule-text",
            "",
            "--graph",
        ])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();

    // Test with blank string (whitespace only)
    egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "edited_then_approved",
            "--edited-rule-text",
            "   ",
            "--graph",
        ])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_workflow_rule_candidate_has_invalid_triggers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("invalid_trigger.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "workflow_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Workflow candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Always run tests".to_owned()),
            proposed_rule_kind: Some("workflow_rule".to_owned()),
            triggers: Some(vec!["pre_push".to_owned()]),
            action_summary: Some("Run cargo test".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_candidate_missing_scope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("missing_scope.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "missing_scope_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Prefer match".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: None,
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn audit_trail_fails_if_prompt_candidate_mismatches_decision_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("audit_candidate_mismatch.jsonl");

    let policy_id = user_context_stable_id(&["preference", "my_policy"]);
    let dec_id = user_context_stable_id(&["decision", "dec_id"]);
    let prompt_id = user_context_stable_id(&["prompt", "prompt_id"]);
    let cand_id = user_context_stable_id(&["candidate", "cand_id"]);
    let unrelated_cand_id = user_context_stable_id(&["candidate", "unrelated_cand_id"]);

    let mut policy = GraphRecord::node(
        policy_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Rule text".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = policy
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        dec_id,
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Approved decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            candidate_id: Some(cand_id.clone()),
            materialized_record_id: Some(policy_id.clone()),
            prompt_id: Some(prompt_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut prompt = GraphRecord::node(
        prompt_id,
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = prompt
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(unrelated_cand_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let obs_id = agent_memory_stable_id(&["obs", "my_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Obs text".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
    }

    let mut candidate = GraphRecord::node(
        cand_id,
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(obs_id.clone()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut unrelated_candidate = GraphRecord::node(
        unrelated_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Unrelated Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = unrelated_candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Unrelated Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(obs_id.clone()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(policy);
    graph.push(decision);
    graph.push(prompt);
    graph.push(candidate);
    graph.push(unrelated_candidate);
    graph.push(obs);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args(["query", "audit", &policy_id, "--graph"])
        .arg(&graph_path)
        .assert()
        .failure();
}

#[test]
fn decide_preference_hashes_redacted_rule_text() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("redacted_hash.jsonl");

    let rule_text = "Use credential ghp_000000000000000000000000000000000000";

    let cand_id = user_context_stable_id(&["candidate", "redacted_hash_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some(rule_text.to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let output_jsonl = fs::read_to_string(&out_path).unwrap();
    let records: Vec<GraphRecord> = output_jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL line"))
        .collect();

    let pref_node = records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Preference,
                    ..
                }
            )
        })
        .unwrap();

    let decision_node = records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    ..
                }
            )
        })
        .unwrap();

    if let (
        GraphRecord::Node {
            id: pref_id,
            user_context: pref_fields,
            ..
        },
        GraphRecord::Node {
            id: decision_id, ..
        },
    ) = (pref_node, decision_node)
    {
        let redacted_text = pref_fields.rule_text.as_ref().expect("missing rule_text");
        assert!(redacted_text.starts_with("<REDACTED:api_token:"));

        let mut hasher = blake3::Hasher::new();
        for part in &[redacted_text.as_bytes(), b"", decision_id.as_bytes()] {
            hasher.update(part);
            hasher.update(b"\0");
        }
        let expected_hash = hasher.finalize().to_hex();
        let expected_pref_id = user_context_stable_id(&["preference", expected_hash.as_str()]);

        assert_eq!(pref_id, &expected_pref_id);
    } else {
        panic!("Missing expected nodes");
    }
}
