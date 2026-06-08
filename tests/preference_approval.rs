#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::redundant_clone,
    clippy::unnecessary_unwrap,
    clippy::similar_names
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

fn make_candidate_valid(candidate: &mut GraphRecord, records: &mut Vec<GraphRecord>) {
    let (cand_id, user_context) = match candidate {
        GraphRecord::Node {
            id,
            confidence,
            evidence_quality,
            user_context,
            ..
        } => {
            *confidence = Some("0.9".to_owned());
            *evidence_quality = Some("verbatim".to_owned());
            (id.clone(), user_context)
        }
        _ => panic!("Not a node"),
    };

    let obs1_id = format!("agent_memory:v1:{cand_id}-obs-1");
    let obs2_id = format!("agent_memory:v1:{cand_id}-obs-2");
    let obs3_id = format!("agent_memory:v1:{cand_id}-obs-3");

    user_context.supporting_evidence = Some(vec![
        EvidenceLink {
            target_record_id: Some(obs1_id.clone()),
            target_domain: "agent_memory".to_owned(),
            relation: "PROPOSED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        },
        EvidenceLink {
            target_record_id: Some(obs2_id.clone()),
            target_domain: "agent_memory".to_owned(),
            relation: "PROPOSED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        },
        EvidenceLink {
            target_record_id: Some(obs3_id.clone()),
            target_domain: "agent_memory".to_owned(),
            relation: "PROPOSED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        },
    ]);
    if user_context.contradicting_evidence.is_none() {
        user_context.contradicting_evidence = Some(vec![]);
    }

    let make_obs = |id: String, session: &str| {
        let mut obs = GraphRecord::node(
            id,
            NodeKind::Observation,
            None,
            None,
            None,
            "Target Obs".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            session_id,
            evidence_links,
            agent_id,
            agent_kind,
            observed_at,
            ingested_at,
            confidence,
            text,
            ..
        } = &mut obs
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *session_id = Some(session.to_owned());
            *agent_id = Some("agent_1".to_owned());
            *agent_kind = Some("claude-code".to_owned());
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *text = Some("Target Observation Text".to_owned());
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v1:rust:symbol:1".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        obs
    };

    let obs1 = make_obs(obs1_id, "session_1");
    let obs2 = make_obs(obs2_id, "session_1");
    let obs3 = make_obs(obs3_id, "session_2");

    let mock_symbol = GraphRecord::node(
        "codegraph:v1:rust:symbol:1".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("my_symbol".to_owned()),
        "mock symbol".to_owned(),
    );

    records.push(mock_symbol);
    records.push(obs1);
    records.push(obs2);
    records.push(obs3);
}

fn make_candidate_valid_in_graph(candidate: &mut GraphRecord, graph: &mut Graph) {
    let mut records = Vec::new();
    make_candidate_valid(candidate, &mut records);
    for r in records {
        graph.push(r);
    }
}

fn make_prompt_valid(prompt: &mut GraphRecord, candidate_id: &str) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = prompt
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(candidate_id.to_owned()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("PromptText".to_owned()),
            prompted_at: Some("2026-06-01T12:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
}

fn make_decision_valid(
    decision: &mut GraphRecord,
    candidate_id: &str,
    prompt_id: &str,
    materialized_id: &str,
) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(candidate_id.to_owned()),
            prompt_id: Some(prompt_id.to_owned()),
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some(materialized_id.to_owned()),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }
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
            contradicting_evidence: Some(vec![]),
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
            contradicting_evidence: Some(vec![]),
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

    // Expecting Prompt + Decision + Preference = 3 records
    assert_eq!(records.len(), 3);

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

    // Prompt + Decision = 2 records, no Preference record
    assert_eq!(records_rej.len(), 2);
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
        .find(|c| c["id"].as_str() == Some(&new_cand_id));

    // Debounce window defaults to T = 30 days and M = 5 observations.
    // The candidate was just decided, has 0 new observations, so it should be suppressed and filtered out.
    assert!(target.is_none());
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
        .find(|c| c["id"].as_str() == Some(&new_cand_id_1));
    let c2 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id_2));
    let c3 = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(&new_cand_id_3))
        .unwrap();

    assert!(c1.is_none());
    assert!(c2.is_none());
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
    make_candidate_valid_in_graph(&mut name_cand, &mut graph);
    make_candidate_valid_in_graph(&mut const_cand, &mut graph);
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
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                language: Some("rust".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                lifecycle_phase: None,
            }),
            ..UserContextFields::empty()
        };
    }

    let cand_id = user_context_stable_id(&["candidate", "cand_broad"]);
    let prompt_id = user_context_stable_id(&["prompt", "prompt_broad"]);

    let mut candidate = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Broad scope rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                language: Some("rust".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                lifecycle_phase: None,
            }),
            contradicting_evidence: Some(vec![]),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-broad-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-broad-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-broad-3".to_owned()),
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

    let mut prompt = GraphRecord::node(
        prompt_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt node".to_owned(),
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
            prompt_surface: Some("cli".to_owned()),
            prompted_to: Some("operator".to_owned()),
            prompt_text: Some("prompt".to_owned()),
            prompted_at: Some("2026-06-01T12:00:00Z".to_owned()),
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
            prompt_id: Some(prompt_id),
            candidate_id: Some(cand_id),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut obs1 = GraphRecord::node(
        "agent_memory:v1:obs-broad-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut session_id,
        ..
    } = obs1
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *session_id = Some("session_1".to_owned());
    }

    let mut obs2 = GraphRecord::node(
        "agent_memory:v1:obs-broad-2".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node 2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut session_id,
        ..
    } = obs2
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *session_id = Some("session_1".to_owned());
    }

    let mut obs3 = GraphRecord::node(
        "agent_memory:v1:obs-broad-3".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node 3".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut session_id,
        ..
    } = obs3
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *session_id = Some("session_2".to_owned());
    }

    let mut graph = Graph::new();
    graph.push(pref);
    graph.push(decision);
    graph.push(prompt);
    graph.push(candidate);
    graph.push(obs1);
    graph.push(obs2);
    graph.push(obs3);
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
        .find(|c| c["id"].as_str() == Some(&new_cand_id));
    // It should be debounced based on the latest terminal rejection decision (not deferred) and filtered out
    assert!(target.is_none());
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
    make_candidate_valid_in_graph(&mut cand, &mut graph);
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
    make_candidate_valid_in_graph(&mut cand, &mut graph);
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
    make_candidate_valid_in_graph(&mut cand, &mut graph);
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

#[test]
fn decide_fails_if_candidate_suppressed_by_debounce() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("debounce_graph.jsonl");

    let old_cand_id = user_context_stable_id(&["candidate", "old_cand"]);
    let new_cand_id = user_context_stable_id(&["candidate", "new_cand"]);

    let mut old_cand = GraphRecord::node(
        old_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Old Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = old_cand
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

    let mut new_cand = GraphRecord::node(
        new_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "New Candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut superseded_by,
        ref mut valid_time,
        ..
    } = new_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-05T12:00:00Z".to_owned());
        *superseded_by = Some(old_cand_id.clone());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    // Reject old_cand to seed the rejection decision
    let mut decision = GraphRecord::node(
        user_context_stable_id(&["decision", "old_decision"]),
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
    } = decision
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(old_cand_id.clone()),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-05T10:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(old_cand);
    graph.push(new_cand);
    graph.push(decision);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Try to decide on new_cand (should fail because it's suppressed)
    let out_path = temp.path().join("out.jsonl");
    egregore()
        .args(["decide", &new_cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .failure();
}

#[test]
fn decide_fails_if_candidate_already_has_terminal_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("terminal_decision_graph.jsonl");

    let cand_id = user_context_stable_id(&["candidate", "my_cand"]);
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
            proposed_rule_text: Some("Rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    // Prior approved decision
    let mut decision = GraphRecord::node(
        user_context_stable_id(&["decision", "prior_dec"]),
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
            candidate_id: Some(cand_id.clone()),
            outcome: Some("approved".to_owned()),
            decided_at: Some("2026-06-05T10:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    graph.push(decision);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Trying to run decide on it again must fail
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
fn audit_trail_fails_if_prompt_hop_is_not_promotion_prompt() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("mismatch_prompt_kind.jsonl");

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

    // Injected prompt node is NOT PromotionPrompt (e.g. Constraint node instead)
    let mut prompt = GraphRecord::node(
        prompt_id,
        NodeKind::Constraint,
        None,
        None,
        None,
        "Constraint".to_owned(),
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

    let mut graph = Graph::new();
    graph.push(policy);
    graph.push(decision);
    graph.push(prompt);
    graph.push(candidate);
    graph.push(obs);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args(["query", "audit", &policy_id, "--graph"])
        .arg(&graph_path)
        .assert()
        .failure();
}

#[test]
fn audit_trail_fails_if_candidate_hop_is_not_promote_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("mismatch_cand_kind.jsonl");

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
            candidate_id: Some(cand_id.clone()),
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

    // Injected candidate is NOT PromoteCandidate (e.g. Preference instead)
    let mut candidate = GraphRecord::node(
        cand_id,
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference Candidate".to_owned(),
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

    let mut graph = Graph::new();
    graph.push(policy);
    graph.push(decision);
    graph.push(prompt);
    graph.push(candidate);
    graph.push(obs);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args(["query", "audit", &policy_id, "--graph"])
        .arg(&graph_path)
        .assert()
        .failure();
}

#[test]
fn test_latest_decision_compares_non_utc_timestamps_correctly() {
    let cand_id = "my_candidate";
    let mut dec_1 = GraphRecord::node(
        "decision_1".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = dec_1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-02T00:30:00+02:00".to_owned()), // June 1, 22:30 UTC
            ..UserContextFields::empty()
        };
    }

    let mut dec_2 = GraphRecord::node(
        "decision_2".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision 2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = dec_2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("approved".to_owned()),
            decided_at: Some("2026-06-01T23:00:00Z".to_owned()), // June 1, 23:00 UTC (newer)
            ..UserContextFields::empty()
        };
    }

    let records = vec![dec_1, dec_2];
    let latest =
        aletheia_egregore::query::latest_decision_for_candidate(&records, cand_id).unwrap();
    assert_eq!(latest.id(), "decision_2");
}

#[test]
fn test_decide_collapses_same_id_revocation_targets() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let target_id = user_context_stable_id(&["preference", "target_policy"]);
    let cand_id = "revocation_candidate";

    // 1. Initial active target policy
    let mut policy_active = GraphRecord::node(
        target_id.clone(),
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
    } = policy_active
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Clean rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    // 2. Later record showing it was already revoked/inactive
    let mut policy_revoked = GraphRecord::node(
        target_id.clone(),
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
    } = policy_revoked
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("Clean rule text".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            active_to: Some("2026-06-04T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    // 3. Candidate node proposing revocation
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("Clean rule text".to_owned()),
            proposed_rule_kind: Some("revocation".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(target_id.clone()),
                target_domain: "user_context".to_owned(),
                relation: "CONTRADICTS".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![policy_active, policy_revoked];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        decided_by: "agent_1".to_owned(),
        rationale: Some("Revoking it again".to_owned()),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        edited_rule_text: None,
        transaction_time: Some("2026-06-05T12:00:00Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("already inactive/revoked"));
}

#[test]
fn test_decide_stamps_redacted_outputs_with_policy_version() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "secret_candidate";

    // 1. Candidate node proposing preference rule with a secret (API Key format)
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate with secret".to_owned(),
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
            proposed_rule_text: Some(
                "Use API Key: sk-12345678901234567890123456789012 for tests".to_owned(),
            ),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        decided_by: "agent_1".to_owned(),
        rationale: Some("Rationale with secret: sk-00000000000000000000000000000000".to_owned()),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        edited_rule_text: None,
        transaction_time: Some("2026-06-05T12:00:00Z".to_owned()),
    };

    let result = decide_candidate(&records, &req).unwrap();

    let mut preference_checked = false;
    let mut decision_checked = false;

    for rec in result {
        if let GraphRecord::Node {
            kind,
            redaction_policy_version,
            ..
        } = rec
        {
            if kind == NodeKind::Preference {
                assert_eq!(redaction_policy_version, Some("v1".to_owned()));
                preference_checked = true;
            } else if kind == NodeKind::PromotionDecision {
                assert_eq!(redaction_policy_version, Some("v1".to_owned()));
                decision_checked = true;
            }
        }
    }

    assert!(preference_checked);
    assert!(decision_checked);
}

#[test]
fn test_decide_stamps_naming_edited_with_redaction() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "naming_candidate";

    // Candidate node proposing naming decision
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate naming".to_owned(),
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
            proposed_rule_text: Some("MyName".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("function".to_owned()),
            canonical_name: Some("MyName".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "edited_then_approved".to_owned(),
        decided_by: "agent_1".to_owned(),
        rationale: Some("Redact sk-12345678901234567890123456789012".to_owned()),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        edited_rule_text: Some("Secret sk-12345678901234567890123456789012".to_owned()),
        transaction_time: None,
    };

    let result = decide_candidate(&records, &req).unwrap();

    let mut naming_checked = false;
    for rec in result {
        if let GraphRecord::Node {
            kind: NodeKind::NamingDecision,
            user_context,
            redaction_policy_version,
            ..
        } = rec
        {
            assert_eq!(redaction_policy_version, Some("v1".to_owned()));
            let name = user_context.canonical_name.as_deref().unwrap();
            assert!(aletheia_egregore::redaction::is_redacted(name));
            naming_checked = true;
        }
    }
    assert!(naming_checked);
}

#[test]
fn test_decide_synthesizes_edges_for_embedded_write() {
    use aletheia_egregore::decide::{
        DecideRequest, decide_candidate, synthesize_user_context_edges,
    };

    let cand_id = "edge_candidate";

    // Candidate node
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate with scope".to_owned(),
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
            proposed_rule_text: Some("MyPreferenceRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        decided_by: "agent_1".to_owned(),
        rationale: Some("Approve this rule".to_owned()),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        edited_rule_text: None,
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let generated = decide_candidate(&records, &req).unwrap();
    let edges = synthesize_user_context_edges(&records, &generated);

    let mut prompted_for = false;
    let mut decided_on = false;
    let mut materialized_as = false;

    for edge in edges {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = edge
        {
            match label {
                aletheia_egregore::ir::EdgeLabel::PromptedFor => {
                    assert_eq!(target, cand_id);
                    prompted_for = true;
                }
                aletheia_egregore::ir::EdgeLabel::DecidedOn => {
                    assert_eq!(target, cand_id);
                    decided_on = true;
                }
                aletheia_egregore::ir::EdgeLabel::MaterializedAs => {
                    assert_eq!(
                        source,
                        "user_context:v1:c089f862e6fab45ed12e38ae62308ded4ad27f8e1b1daf3943f3b0c727be0a82"
                    );
                    materialized_as = true;
                }
                _ => {}
            }
        }
    }

    assert!(prompted_for);
    assert!(decided_on);
    assert!(materialized_as);
}

#[test]
fn test_decide_rejects_non_agent_evidence_in_audit() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "bad_evidence_candidate";
    let decision_id = "decision_1";
    let prompt_id = "prompt_1";

    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate with bad evidence".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("MyRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![]),
            supporting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some("some_preference".to_owned()),
                target_domain: "user_context".to_owned(), // WRONG! should be agent_memory
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

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
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
            candidate_id: Some(cand_id.to_owned()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("PromptText".to_owned()),
            prompted_at: Some("2026-06-05T12:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        decision_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
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
            candidate_id: Some(cand_id.to_owned()),
            prompt_id: Some(prompt_id.to_owned()),
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some("some_preference".to_owned()),
            decided_at: Some("2026-06-05T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut preference = GraphRecord::node(
        "some_preference".to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = preference
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            approval_decision_id: Some(decision_id.to_owned()),
            rule_text: Some("MyRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            active_from: Some("2026-06-05T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![
        candidate.clone(),
        prompt.clone(),
        decision.clone(),
        preference.clone(),
    ];
    let result = audit_trail(&records, &preference);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("target domain"));

    // Fix domain, but keep relation wrong
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.supporting_evidence.as_mut().unwrap()[0].target_domain =
            "agent_memory".to_owned();
        user_context.supporting_evidence.as_mut().unwrap()[0].relation = "OBSERVES".to_owned(); // WRONG! should be PROPOSED_BY
    }
    let records = vec![
        candidate.clone(),
        prompt.clone(),
        decision.clone(),
        preference.clone(),
    ];
    let result = audit_trail(&records, &preference);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("relation"));

    // Fix relation, but make target kind wrong (e.g. referencing a Preference instead of Observation/AgentTurn/Decision)
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.supporting_evidence.as_mut().unwrap()[0].relation = "PROPOSED_BY".to_owned();
        user_context.supporting_evidence.as_mut().unwrap()[0].target_record_id =
            Some("some_preference".to_owned()); // WRONG kind!
    }
    let records = vec![candidate, prompt, decision, preference.clone()];
    let result = audit_trail(&records, &preference);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("invalid node kind"));
}

#[test]
fn test_scoped_policy_query_excludes_missing_scopes() {
    use aletheia_egregore::query::active_policy;

    let mut preference = GraphRecord::node(
        "pref_no_scope".to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Pref".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = preference
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            approval_decision_id: Some("dec_1".to_owned()),
            scope: None, // Missing scope!
            rule_text: Some("Clean text".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        "dec_1".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
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
            materialized_record_id: Some("pref_no_scope".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![preference, decision];
    let query_scope = UserContextScope {
        repo: Some("myrepo".to_owned()),
        ..UserContextScope::default()
    };

    // Scoped query: should exclude the preference since it lacks a scope
    let results = active_policy(&records, Some(&query_scope));
    assert!(results.is_empty());
}

#[test]
fn test_audit_trail_verifies_candidate_body_and_kind() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "candidate_1";
    let decision_id = "decision_1";
    let prompt_id = "prompt_1";

    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Secret: ghp_123456789012345678901234567890123456".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![]),
            supporting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
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
            candidate_id: Some(cand_id.to_owned()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("PromptText".to_owned()),
            prompted_at: Some("2026-06-05T12:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        decision_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
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
            candidate_id: Some(cand_id.to_owned()),
            prompt_id: Some(prompt_id.to_owned()),
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some("pref_1".to_owned()),
            decided_at: Some("2026-06-05T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut preference = GraphRecord::node(
        "pref_1".to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = preference
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            approval_decision_id: Some(decision_id.to_owned()),
            rule_text: Some("MismatchedText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            active_from: Some("2026-06-05T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![
        candidate.clone(),
        prompt.clone(),
        decision.clone(),
        preference.clone(),
    ];
    let result = audit_trail(&records, &preference);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("does not match"));

    // Fix body but change candidate's proposed_rule_kind to mismatch the durable kind (Constraint)
    let mut bad_kind_preference = preference.clone();
    if let GraphRecord::Node { user_context, .. } = &mut bad_kind_preference {
        let cand_text = match &candidate {
            GraphRecord::Node { user_context, .. } => {
                user_context.proposed_rule_text.as_deref().unwrap()
            }
            _ => unreachable!(),
        };
        user_context.rule_text = Some(aletheia_egregore::redaction::redact_value(cand_text));
    }
    let mut bad_kind_candidate = candidate.clone();
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = bad_kind_candidate
    {
        user_context.proposed_rule_kind = Some("constraint".to_owned()); // WRONG kind (constraint vs Preference)
    }
    let records = vec![
        bad_kind_candidate,
        prompt.clone(),
        decision.clone(),
        bad_kind_preference.clone(),
    ];
    let result = audit_trail(&records, &bad_kind_preference);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("proposed rule kind"));
}

#[test]
fn test_decide_normalizes_and_redacts_candidate_body() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "candidate_secret";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("Secret: ghp_123456789012345678901234567890123456".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let generated = decide_candidate(&records, &req).unwrap();

    // The generated vector should contain the updated candidate node which is redacted!
    let updated_cand = generated
        .iter()
        .find(|r| r.id() == cand_id)
        .expect("updated candidate not found in generated records");

    if let GraphRecord::Node {
        user_context,
        redaction_policy_version,
        ..
    } = updated_cand
    {
        let text = user_context.proposed_rule_text.as_deref().unwrap();
        assert!(text.contains("<REDACTED:api_token:"));
        assert_eq!(redaction_policy_version.as_deref(), Some("v1"));
    } else {
        panic!("updated_cand is not a node");
    }
}

#[test]
fn test_decide_naming_hash_uses_redacted_edited_name() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_naming";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("OldName".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("function".to_owned()),
            canonical_name: Some("OldName".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "edited_then_approved".to_owned(),
        edited_rule_text: Some("SecretToken: ghp_123456789012345678901234567890123456".to_owned()),
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let generated = decide_candidate(&records, &req).unwrap();

    // Find the materialized naming decision node
    let naming_node = generated
        .iter()
        .find(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                *kind == NodeKind::NamingDecision
            } else {
                false
            }
        })
        .expect("materialized naming decision not found");

    if let GraphRecord::Node {
        id, user_context, ..
    } = naming_node
    {
        let name = user_context.canonical_name.as_deref().unwrap();
        assert!(name.contains("<REDACTED:api_token:"));

        let approval_dec_id = user_context.approval_decision_id.as_deref().unwrap();
        let mut hasher = blake3::Hasher::new();
        for part in &["function", name, "", approval_dec_id] {
            hasher.update(part.as_bytes());
            hasher.update(b"\0");
        }
        let hash = hasher.finalize().to_hex().to_string();
        let expected_id = user_context_stable_id(&["naming_decision", &hash]);
        assert_eq!(id, &expected_id);
    } else {
        panic!("naming_node is not a node");
    }
}

#[test]
fn test_decide_naming_decision_plain_approved_requires_matching_fields() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_naming_mismatch";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("MismatchText".to_owned()),
            proposed_rule_kind: Some("naming_decision".to_owned()),
            entity_kind: Some("function".to_owned()),
            canonical_name: Some("OtherName".to_owned()), // MISMATCH
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("must match canonical_name")
    );
}

#[test]
fn test_decide_cmd_fails_for_invalid_candidate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("invalid_candidate.jsonl");
    let store_path = temp.path().join("store");

    // Create a candidate with NO supporting observations
    let cand_id = user_context_stable_id(&["candidate", "invalid_cand"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Invalid candidate".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            supporting_evidence: Some(vec![]), // Empty! Less than 5.
            ..UserContextFields::empty()
        };
    }

    let mut graph = Graph::new();
    graph.push(cand);
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    // Call egregore decide, writing to store_path. It should fail candidate validation.
    egregore()
        .args(["decide", &cand_id, "--outcome", "approved", "--graph"])
        .arg(&graph_path)
        .arg("--data-dir")
        .arg(&store_path)
        .assert()
        .failure();
}

#[test]
fn test_decide_fails_for_empty_operator_fields() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_op_validation";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![candidate];

    // Empty prompted_to
    let req_empty_prompted = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: " ".to_owned(), // Empty/whitespace
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };
    let result = decide_candidate(&records, &req_empty_prompted);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("prompted_to cannot be empty")
    );

    // Empty decided_by
    let req_empty_decided = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: " \t ".to_owned(), // Empty/whitespace
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };
    let result = decide_candidate(&records, &req_empty_decided);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("decided_by cannot be empty")
    );
}

#[test]
fn test_decide_redacts_candidate_action_summary() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_action_redaction";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("workflow_rule".to_owned()),
            scope: Some(UserContextScope::default()),
            triggers: Some(vec!["pre_commit".to_owned()]),
            action_summary: Some("Execute task using key ghp_12345678901234567890".to_owned()), // Contains secret
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate.clone());
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let generated = decide_candidate(&records, &req).unwrap();

    // Find the updated PromoteCandidate record in generated
    let updated_cand = generated
        .iter()
        .find(|r| r.id() == cand_id)
        .expect("updated candidate should be emitted");
    if let GraphRecord::Node {
        user_context,
        redaction_policy_version,
        ..
    } = updated_cand
    {
        let action = user_context.action_summary.as_deref().unwrap();
        assert!(action.contains("api_token:"));
        assert!(!action.contains("ghp_12345678901234567890"));
        assert_eq!(redaction_policy_version.as_deref(), Some("v1"));
    } else {
        panic!("Not a node");
    }
}

#[test]
fn test_decide_revocation_rejects_if_any_copy_revoked() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let target_id = "target_policy_revocation_copies";
    let active_copy = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Active preference".to_owned(),
    );
    let mut revoked_copy = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Revoked preference".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = revoked_copy
    {
        user_context.active_to = Some("2026-06-01T12:00:00Z".to_owned());
    }

    let cand_id = "cand_revoke_copies";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("revocation".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(target_id.to_owned()),
                target_domain: "user_context".to_owned(),
                relation: "CONTRADICTS".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![revoked_copy, active_copy];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("is already inactive/revoked")
    );
}

#[test]
fn test_decide_fails_for_empty_proposed_rule_body() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_empty_proposed_body";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate with empty proposed body".to_owned(),
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
            proposed_rule_text: Some(" \t\n ".to_owned()), // Whitespace only
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("cannot be empty or whitespace-only")
    );
}

#[test]
fn test_active_policy_requires_valid_approval_chain() {
    use aletheia_egregore::query::active_policy;

    let policy_id = "preference_invalid_chain";
    let mut policy = GraphRecord::node(
        policy_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = policy
    {
        *user_context = UserContextFields {
            rule_text: Some("PreferenceText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some("decision_invalid".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        "decision_invalid".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = decision
    {
        *user_context = UserContextFields {
            outcome: Some("rejected".to_owned()),
            materialized_record_id: Some(policy_id.to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![policy, decision];
    let active = active_policy(&records, None);
    assert!(active.is_empty());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_copied_evidence_record_validation() {
    use aletheia_egregore::daemon::validate_agent_memory_record_for_cli;

    let obs_id = "agent_memory:v1:obs-invalid".to_owned();
    let mut observation = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut agent_id,
        ref mut agent_kind,
        ref mut session_id,
        ref mut observed_at,
        ref mut ingested_at,
        ..
    } = observation
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *agent_id = Some("agent_1".to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *session_id = Some("session_1".to_owned());
        *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
        *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
    }

    let records = vec![observation.clone()];
    let temp = tempfile::tempdir().unwrap();
    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(temp.path()).unwrap();

    let result = validate_agent_memory_record_for_cli(&observation, &records, &sink);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("confidence")
            || err_msg.contains("text")
            || err_msg.contains("evidence_links")
    );
}

#[test]
fn test_decide_revocation_requires_approved_policy() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let target_id = "target_policy_unapproved";
    let target = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Unapproved preference".to_owned(),
    );

    let cand_id = "cand_revoke_unapproved";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("revocation".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(target_id.to_owned()),
                target_domain: "user_context".to_owned(),
                relation: "CONTRADICTS".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![target];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("has no valid approval chain")
    );
}

#[test]
fn test_audit_trail_validates_active_from_match() {
    use aletheia_egregore::query::audit_trail;

    let policy_id = "pref_mismatched_active_from";
    let mut policy = GraphRecord::node(
        policy_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = policy
    {
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some("decision_mismatch".to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        "decision_mismatch".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision node".to_owned(),
    );
    make_decision_valid(&mut decision, "cand_mismatch", "prompt_mismatch", policy_id);
    if let GraphRecord::Node { user_context, .. } = &mut decision {
        user_context.decided_at = Some("2026-06-01T12:05:00Z".to_owned());
    }

    let mut prompt = GraphRecord::node(
        "prompt_mismatch".to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt node".to_owned(),
    );
    make_prompt_valid(&mut prompt, "cand_mismatch");

    let mut candidate = GraphRecord::node(
        "cand_mismatch".to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate node".to_owned(),
    );
    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);

    records.push(policy.clone());
    records.push(decision);
    records.push(prompt);
    records.push(candidate);

    let result = audit_trail(&records, &policy);
    assert!(result.is_err());
    assert!(result.err().unwrap().contains("does not match decision"));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_decide_copies_superseded_rejection_lineage() {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("lineage.jsonl");
    let db_path = temp.path().join("store");

    let cand_a_id = user_context_stable_id(&["candidate", "cand_a"]);
    let cand_b_id = user_context_stable_id(&["candidate", "cand_b"]);

    let mut cand_a = GraphRecord::node(
        cand_a_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate A".to_owned(),
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
    } = cand_a
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText A".to_owned()),
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
            ]),
            contradicting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }

    let prompt_a_id = user_context_stable_id(&["prompt", "prompt_a"]);
    let mut prompt_a = GraphRecord::node(
        prompt_a_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt A".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = prompt_a
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_a_id.clone()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("Do you approve?".to_owned()),
            prompted_at: Some("2026-06-01T12:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let dec_a_id = user_context_stable_id(&["decision", "dec_a"]);
    let mut dec_a = GraphRecord::node(
        dec_a_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision A".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = dec_a
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            outcome: Some("rejected".to_owned()),
            candidate_id: Some(cand_a_id.clone()),
            prompt_id: Some(prompt_a_id.clone()),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut cand_b = GraphRecord::node(
        cand_b_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut superseded_by,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = cand_b
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *superseded_by = Some(cand_a_id.clone());
        *valid_time = Some("2026-07-05T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText B".to_owned()),
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
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:obs-8".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
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

    let mut observations = Vec::new();
    for i in 1..=8 {
        let obs_id = format!("agent_memory:v1:obs-{i}");
        let mut o = GraphRecord::node(
            obs_id.clone(),
            NodeKind::Observation,
            None,
            None,
            None,
            format!("Observation node {i}").to_owned(),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            ref mut domain,
            ref mut text,
            ref mut confidence,
            ref mut agent_id,
            ref mut agent_kind,
            ref mut session_id,
            ref mut observed_at,
            ref mut ingested_at,
            ref mut evidence_links,
            ..
        } = o
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *text = Some(format!("Observed pattern {i}").to_owned());
            *confidence = Some("1.0".to_owned());
            *agent_id = Some("agent_1".to_owned());
            *agent_kind = Some("claude-code".to_owned());
            *session_id = Some(if i == 1 || i == 2 {
                "session_1".to_owned()
            } else {
                "session_2".to_owned()
            });
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v1:rust:symbol:1".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        observations.push(o);
    }

    let mock_symbol = GraphRecord::node(
        "codegraph:v1:rust:symbol:1".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("my_symbol".to_owned()),
        "mock symbol".to_owned(),
    );
    let mut graph = Graph::new();
    graph.push(mock_symbol);
    graph.push(cand_a);
    graph.push(prompt_a);
    graph.push(dec_a);
    graph.push(cand_b);
    for o in observations {
        graph.push(o);
    }
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args([
            "decide",
            &cand_b_id,
            "--outcome",
            "approved",
            "--graph",
            graph_path.to_str().unwrap(),
            "--data-dir",
            db_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&db_path).unwrap();
    let records = sink.read_all_records().unwrap();

    assert!(records.iter().any(|r| r.id() == cand_a_id));
    assert!(records.iter().any(|r| r.id() == cand_b_id));
    assert!(records.iter().any(|r| r.id() == dec_a_id));
    for i in 1..=6 {
        assert!(
            records
                .iter()
                .any(|r| r.id() == format!("agent_memory:v1:obs-{i}"))
        );
    }
}

#[test]
fn test_audit_trail_threshold_validation() {
    use aletheia_egregore::query::audit_trail;

    let policy_id = "pref_thresholds";
    let mut policy = GraphRecord::node(
        policy_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = policy
    {
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some("decision_1".to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut decision = GraphRecord::node(
        "decision_1".to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision node".to_owned(),
    );
    make_decision_valid(&mut decision, "cand_1", "prompt_1", policy_id);

    let mut prompt = GraphRecord::node(
        "prompt_1".to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt node".to_owned(),
    );
    make_prompt_valid(&mut prompt, "cand_1");

    let make_obs = |id: &str, session: &str| {
        let mut obs = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "Obs".to_owned(),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            ref mut domain,
            ref mut session_id,
            ..
        } = obs
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *session_id = Some(session.to_owned());
        }
        obs
    };

    let obs1 = make_obs("agent_memory:v1:obs-1", "sess_1");
    let obs2 = make_obs("agent_memory:v1:obs-2", "sess_1");
    let obs3 = make_obs("agent_memory:v1:obs-3", "sess_2");

    // Case 1: 2 observations from 2 sessions -> fails (unique observations < 3)
    let mut candidate = GraphRecord::node(
        "cand_1".to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate node".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![]),
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
                    target_record_id: Some("agent_memory:v1:obs-3".to_owned()),
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

    let records = vec![
        policy.clone(),
        decision.clone(),
        prompt.clone(),
        candidate.clone(),
        obs1.clone(),
        obs3.clone(),
    ];
    let res = audit_trail(&records, &policy);
    assert!(res.is_err());
    assert!(
        res.err()
            .unwrap()
            .contains("has 2 unique supporting observations")
    );

    // Case 2: 3 observations from 1 session -> fails (distinct sessions < 2)
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.supporting_evidence = Some(vec![
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
                target_record_id: Some("agent_memory:v1:obs-broad-other-duplicated".to_owned()),
                target_domain: "agent_memory".to_owned(),
                relation: "PROPOSED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
        ]);
    }
    let obs_dup = make_obs("agent_memory:v1:obs-broad-other-duplicated", "sess_1");
    let records = vec![
        policy.clone(),
        decision.clone(),
        prompt.clone(),
        candidate.clone(),
        obs1.clone(),
        obs2.clone(),
        obs_dup,
    ];
    let res = audit_trail(&records, &policy);
    assert!(res.is_err());
    assert!(
        res.err()
            .unwrap()
            .contains("evidence from 1 distinct sessions")
    );

    // Case 3: 3 observations from 2 sessions -> succeeds
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.supporting_evidence = Some(vec![
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
        ]);
    }
    let records = vec![
        policy.clone(),
        decision,
        prompt,
        candidate,
        obs1,
        obs2,
        obs3,
    ];
    let res = audit_trail(&records, &policy);
    assert!(res.is_ok());
}

#[test]
fn test_decide_rejects_if_any_prior_decision_terminal() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "test_cand_terminal";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate node".to_owned(),
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
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut d_non_terminal = GraphRecord::node(
        "dec_deferred".to_owned(),
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
    } = d_non_terminal
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("deferred".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut d_terminal = GraphRecord::node(
        "dec_rejected".to_owned(),
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
    } = d_terminal
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("rejected".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![
        candidate.clone(),
        d_non_terminal.clone(),
        d_terminal.clone(),
    ];
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let res = decide_candidate(&records, &req);
    assert!(res.is_err());
    assert!(
        res.err()
            .unwrap()
            .to_string()
            .contains("already has a terminal decision outcome: 'rejected'")
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_decide_copies_revocation_target_approval_chain() {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("revocation_graph.jsonl");
    let db_path = temp.path().join("store");

    let target_pref_id = user_context_stable_id(&["preference", "target_pref"]);
    let target_dec_id = user_context_stable_id(&["decision", "target_dec"]);
    let target_prompt_id = user_context_stable_id(&["prompt", "target_prompt"]);
    let target_cand_id = user_context_stable_id(&["candidate", "target_cand"]);

    let mut target_pref = GraphRecord::node(
        target_pref_id.clone(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Target preference to be revoked".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = target_pref
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("TargetRuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            approval_decision_id: Some(target_dec_id.clone()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut target_dec = GraphRecord::node(
        target_dec_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Target Decision".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = target_dec
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            outcome: Some("approved".to_owned()),
            materialized_record_id: Some(target_pref_id.clone()),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            decided_by: Some("operator".to_owned()),
            prompt_id: Some(target_prompt_id.clone()),
            candidate_id: Some(target_cand_id.clone()),
            ..UserContextFields::empty()
        };
    }

    let mut target_prompt = GraphRecord::node(
        target_prompt_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Target Prompt".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = target_prompt
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(target_cand_id.clone()),
            prompt_surface: Some("cli".to_owned()),
            prompt_text: Some("Target Prompt Text".to_owned()),
            prompted_at: Some("2026-06-01T12:00:00Z".to_owned()),
            prompted_to: Some("operator".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut target_cand = GraphRecord::node(
        target_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Target Candidate".to_owned(),
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
    } = target_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("TargetRuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![]),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:target-obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:target-obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:target-obs-3".to_owned()),
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

    let make_obs = |id: &str, session: &str| {
        let mut obs = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "Target Obs".to_owned(),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            ref mut domain,
            ref mut session_id,
            ref mut evidence_links,
            ref mut agent_id,
            ref mut agent_kind,
            ref mut observed_at,
            ref mut ingested_at,
            ref mut confidence,
            ref mut text,
            ..
        } = obs
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *session_id = Some(session.to_owned());
            *agent_id = Some("agent_1".to_owned());
            *agent_kind = Some("claude-code".to_owned());
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *text = Some("Target Observation Text".to_owned());
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v1:rust:symbol:1".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        obs
    };
    let target_obs1 = make_obs("agent_memory:v1:target-obs-1", "session_1");
    let target_obs2 = make_obs("agent_memory:v1:target-obs-2", "session_1");
    let target_obs3 = make_obs("agent_memory:v1:target-obs-3", "session_2");

    let revoke_cand_id = user_context_stable_id(&["candidate", "revoke_cand"]);
    let mut revoke_cand = GraphRecord::node(
        revoke_cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Revocation Candidate".to_owned(),
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
    } = revoke_cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("TargetRuleText".to_owned()),
            proposed_rule_kind: Some("revocation".to_owned()),
            scope: Some(UserContextScope::default()),
            contradicting_evidence: Some(vec![EvidenceLink {
                target_record_id: Some(target_pref_id.clone()),
                target_domain: "user_context".to_owned(),
                relation: "CONTRADICTS".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:revoke-obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:revoke-obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("agent_memory:v1:revoke-obs-3".to_owned()),
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

    let revoke_obs1 = make_obs("agent_memory:v1:revoke-obs-1", "session_3");
    let revoke_obs2 = make_obs("agent_memory:v1:revoke-obs-2", "session_3");
    let revoke_obs3 = make_obs("agent_memory:v1:revoke-obs-3", "session_4");

    let mock_symbol = GraphRecord::node(
        "codegraph:v1:rust:symbol:1".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("my_symbol".to_owned()),
        "mock symbol".to_owned(),
    );
    let mut graph = Graph::new();
    graph.push(mock_symbol);
    graph.push(target_pref.clone());
    graph.push(target_dec.clone());
    graph.push(target_prompt.clone());
    graph.push(target_cand.clone());
    graph.push(target_obs1.clone());
    graph.push(target_obs2.clone());
    graph.push(target_obs3.clone());
    graph.push(revoke_cand.clone());
    graph.push(revoke_obs1.clone());
    graph.push(revoke_obs2.clone());
    graph.push(revoke_obs3.clone());

    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args([
            "decide",
            &revoke_cand_id,
            "--outcome",
            "approved",
            "--graph",
            graph_path.to_str().unwrap(),
            "--data-dir",
            db_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&db_path).unwrap();
    let db_records = sink.read_all_records().unwrap();

    assert!(db_records.iter().any(|r| r.id() == revoke_cand_id));
    assert!(db_records.iter().any(|r| r.id() == target_pref_id));
    assert!(db_records.iter().any(|r| r.id() == target_dec_id));
    assert!(db_records.iter().any(|r| r.id() == target_prompt_id));
    assert!(db_records.iter().any(|r| r.id() == target_cand_id));
    assert!(
        db_records
            .iter()
            .any(|r| r.id() == "agent_memory:v1:target-obs-1")
    );
    assert!(
        db_records
            .iter()
            .any(|r| r.id() == "agent_memory:v1:target-obs-2")
    );
    assert!(
        db_records
            .iter()
            .any(|r| r.id() == "agent_memory:v1:target-obs-3")
    );
}

#[test]
fn test_decide_fails_if_superseded_candidate_has_no_rejection_decision() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_a_id = user_context_stable_id(&["candidate", "cand_a_no_reject"]);
    let cand_b_id = user_context_stable_id(&["candidate", "cand_b_superseding"]);

    let mut cand_a = GraphRecord::node(
        cand_a_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate A".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = cand_a
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText A".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let mut cand_b = GraphRecord::node(
        cand_b_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut superseded_by,
        ref mut user_context,
        ..
    } = cand_b
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *superseded_by = Some(cand_a_id.clone());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText B".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![cand_a];
    make_candidate_valid(&mut cand_b, &mut records);
    records.push(cand_b);

    let req = DecideRequest {
        candidate_id: cand_b_id,
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("must reference a rejected candidate"));
}

#[test]
fn test_decide_fails_if_supporting_evidence_has_empty_session_id() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_empty_session";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Prefer matches".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            supporting_evidence: Some(vec![
                EvidenceLink {
                    target_record_id: Some("obs-1".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("obs-2".to_owned()),
                    target_domain: "agent_memory".to_owned(),
                    relation: "PROPOSED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("obs-3".to_owned()),
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

    // Prepare observations where one has an empty session_id
    let obs1 = {
        let mut n = GraphRecord::node(
            "obs-1".to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "Obs 1".to_owned(),
        );
        if let GraphRecord::Node {
            domain,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            schema_version,
            ..
        } = &mut n
        {
            *domain = Some("agent_memory".to_owned());
            *session_id = Some("sess-1".to_owned());
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        n
    };
    let obs2 = {
        let mut n = GraphRecord::node(
            "obs-2".to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "Obs 2".to_owned(),
        );
        if let GraphRecord::Node {
            domain,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            schema_version,
            ..
        } = &mut n
        {
            *domain = Some("agent_memory".to_owned());
            *session_id = Some(String::new()); // EMPTY SESSION ID
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        n
    };
    let obs3 = {
        let mut n = GraphRecord::node(
            "obs-3".to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "Obs 3".to_owned(),
        );
        if let GraphRecord::Node {
            domain,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            schema_version,
            ..
        } = &mut n
        {
            *domain = Some("agent_memory".to_owned());
            *session_id = Some("sess-2".to_owned());
            *observed_at = Some("2026-06-01T12:00:00Z".to_owned());
            *ingested_at = Some("2026-06-01T12:00:00Z".to_owned());
            *confidence = Some("1.0".to_owned());
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        n
    };

    let records = vec![candidate, obs1, obs2, obs3];
    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("supporting_evidence.session_id is required"));
}

#[test]
fn test_decide_fails_if_contradicting_evidence_is_missing() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_missing_contr";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("Prefer matches".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            contradicting_evidence: None,
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.contradicting_evidence = None;
    }
    records.push(candidate);

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("contradicting_evidence"));
}

#[test]
fn test_decide_fails_if_edited_then_approved_lacks_proposed_rule_text() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};

    let cand_id = "cand_missing_rule_text";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: None,
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope {
                repo: Some("egregore".to_owned()),
                path_glob: Some("src/**/*.rs".to_owned()),
                language: Some("rust".to_owned()),
                lifecycle_phase: Some("pre_commit".to_owned()),
            }),
            contradicting_evidence: Some(vec![]),
            ..UserContextFields::empty()
        };
    }

    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    if let GraphRecord::Node {
        ref mut user_context,
        ..
    } = candidate
    {
        user_context.proposed_rule_text = None;
    }
    records.push(candidate);

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "edited_then_approved".to_owned(),
        edited_rule_text: Some("Edited Rule Text".to_owned()),
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00.000Z".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("proposed_rule_text"));
}

#[test]
fn test_pending_candidates_excludes_any_terminal_decision() {
    use aletheia_egregore::query::pending_candidates;

    let cand_id = "cand_terminal_dec";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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

    let dec1_id = "decision_1";
    let mut dec1 = GraphRecord::node(
        dec1_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = dec1
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("rejected".to_owned()),
            decided_at: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let dec2_id = "decision_2";
    let mut dec2 = GraphRecord::node(
        dec2_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision 2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = dec2
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(cand_id.to_owned()),
            outcome: Some("deferred".to_owned()),
            decided_at: Some("2026-06-02T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let records = vec![candidate, dec1, dec2];
    let pending = pending_candidates(&records, None);
    assert!(
        pending.is_empty(),
        "Candidate should be excluded from pending list because it has a terminal decision"
    );
}

#[test]
fn test_audit_trail_validates_candidate_metadata() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_audit_validation";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
        ref mut confidence,
        ref mut evidence_quality,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *confidence = None;
        *evidence_quality = Some("verbatim".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let pref_id = "pref_audit";
    let dec_id = "dec_audit";
    let prompt_id = "prompt_audit";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);
    if let GraphRecord::Node {
        ref mut confidence, ..
    } = candidate
    {
        *confidence = None;
    }
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("confidence"));
}

#[test]
fn test_decide_fails_for_invalid_transaction_time() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};
    let cand_id = "cand_invalid_time";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let mut records = Vec::new();
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("invalid-time-format".to_owned()),
    };

    let result = decide_candidate(&records, &req);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("Invalid transaction_time format"));
}

#[test]
fn test_audit_trail_fails_for_empty_session_id() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_audit_empty_session";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
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
            proposed_rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }

    let pref_id = "pref_audit_empty_sess";
    let dec_id = "dec_audit_empty_sess";
    let prompt_id = "prompt_audit_empty_sess";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);

    // Make one observation have an empty session_id
    if let Some(GraphRecord::Node { session_id, .. }) = records
        .iter_mut()
        .find(|r| r.id().starts_with("agent_memory:v1:"))
    {
        *session_id = Some(String::new());
    }
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("supporting_evidence.session_id is required"));
}

#[test]
fn test_decide_rightmost_resolution_prefers_redacted() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};
    let cand_id = "cand_rightmost_redacted";

    // First: unredacted candidate (has raw credentials)
    let mut cand_raw = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Raw Candidate".to_owned(),
    );
    let mut records = Vec::new();
    make_candidate_valid(&mut cand_raw, &mut records);
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand_raw
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        user_context.proposed_rule_text =
            Some("Secret: ghp_111111111111111111111111111111111111".to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.scope = Some(UserContextScope::default());
    }

    // Second: redacted candidate (has redacted marker)
    let mut cand_red = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Redacted Candidate".to_owned(),
    );
    make_candidate_valid(&mut cand_red, &mut Vec::new());
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ..
    } = cand_red
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        user_context.proposed_rule_text = Some("Secret: [API_TOKEN]".to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.scope = Some(UserContextScope::default());
    }

    // Push both, raw first, redacted second
    records.push(cand_raw);
    records.push(cand_red);

    let req = DecideRequest {
        candidate_id: cand_id.to_owned(),
        outcome: "approved".to_owned(),
        edited_rule_text: None,
        rationale: None,
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some("2026-06-05T12:00:00Z".to_owned()),
    };

    // If decide_candidate resolves the rightmost redacted candidate, the materialized Preference
    // node will carry "Secret: [API_TOKEN]".
    let result = decide_candidate(&records, &req).unwrap();
    let pref_node = result
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
    if let GraphRecord::Node { user_context, .. } = pref_node {
        assert_eq!(
            user_context.rule_text.as_deref(),
            Some("Secret: [API_TOKEN]")
        );
    } else {
        panic!("Materialized preference not found");
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_cli_decide_copies_references_for_redacted_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("graph.jsonl");
    let db_path = temp.path().join("store");

    let cand_id = user_context_stable_id(&["candidate", "redacted_cand_cli"]);
    let mut cand = GraphRecord::node(
        cand_id.clone(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let mut records = Vec::new();
    make_candidate_valid(&mut cand, &mut records);
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        user_context.proposed_rule_text =
            Some("Secret: ghp_111111111111111111111111111111111111".to_owned());
        user_context.proposed_rule_kind = Some("preference".to_owned());
        user_context.scope = Some(UserContextScope::default());
    }

    let mut graph = Graph::new();
    graph.push(cand);
    for r in records {
        graph.push(r);
    }
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "approved",
            "--graph",
            graph_path.to_str().unwrap(),
            "--data-dir",
            db_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&db_path).unwrap();
    let db_records = sink.read_all_records().unwrap();

    // Verify candidate references (evidence observations) were copied
    for i in 1..=3 {
        let obs_id = format!("agent_memory:v1:{cand_id}-obs-{i}");
        assert!(db_records.iter().any(|r| r.id() == obs_id));
    }

    // Verify that the original raw unredacted candidate body is NOT in the database,
    // only the redacted clone!
    let db_cand = db_records.iter().find(|r| r.id() == cand_id).unwrap();
    if let GraphRecord::Node { user_context, .. } = db_cand {
        assert_eq!(
            user_context.proposed_rule_text.as_deref(),
            Some("<REDACTED:api_token:ddbde35135f9>")
        );
    }
}

#[test]
fn test_audit_trail_fails_for_missing_candidate_contradicting_evidence() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_missing_contradicting";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let pref_id = "pref_missing_contra";
    let dec_id = "dec_missing_contra";
    let prompt_id = "prompt_missing_contra";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);

    if let GraphRecord::Node { user_context, .. } = &mut candidate {
        user_context.scope = Some(UserContextScope::default());
        user_context.contradicting_evidence = None;
    }
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("lacks contradicting_evidence"));
}

#[test]
fn test_audit_trail_fails_for_missing_durable_scope() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_missing_scope";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let pref_id = "pref_missing_scope";
    let dec_id = "dec_missing_scope";
    let prompt_id = "prompt_missing_scope";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: None, // Missing scope!
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("lacks scope"));
}

#[test]
fn test_audit_trail_fails_for_invalid_prompt_metadata() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_invalid_prompt";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let pref_id = "pref_invalid_prompt";
    let dec_id = "dec_invalid_prompt";
    let prompt_id = "prompt_invalid_prompt";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);
    if let GraphRecord::Node { user_context, .. } = &mut prompt {
        user_context.prompt_surface = Some("invalid_surface".to_owned());
    }

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("prompt_surface 'invalid_surface' is invalid"));
}

#[test]
fn test_audit_trail_fails_for_missing_decision_approver() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_missing_approver";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let pref_id = "pref_missing_approver";
    let dec_id = "dec_missing_approver";
    let prompt_id = "prompt_missing_approver";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);
    if let GraphRecord::Node { user_context, .. } = &mut dec {
        user_context.decided_by = None;
    }

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("lacks decided_by"));
}

#[test]
fn test_audit_trail_fails_for_missing_durable_kind_specific_fields() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "cand_missing_kind_fields";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate".to_owned(),
    );
    let wf_id = "wf_missing_kind_fields";
    let dec_id = "dec_wf";
    let prompt_id = "prompt_wf";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, wf_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut wf = GraphRecord::node(
        wf_id.to_owned(),
        NodeKind::WorkflowRule,
        None,
        None,
        None,
        "WorkflowRule".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        user_context,
        ..
    } = &mut wf
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("workflow_rule".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            // missing triggers and action_summary!
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, wf];
    make_candidate_valid(&mut candidate, &mut records);
    if let GraphRecord::Node { user_context, .. } = &mut candidate {
        user_context.proposed_rule_kind = Some("workflow_rule".to_owned());
    }
    records.push(candidate);

    let result = audit_trail(&records, &records[2]);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(err_msg.contains("lacks triggers"));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_cli_decide_copies_agent_memory_evidence_edges() {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("graph.jsonl");
    let db_path = temp.path().join("store");

    let cand_id = user_context_stable_id(&["candidate", "agent_memory_edge_cand"]);
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
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("MyPreferenceRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }
    let mut records = Vec::new();
    make_candidate_valid(&mut cand, &mut records);

    // Customize one of the generated observations to have an evidence link
    // so we can test that the edge is correctly synthesized when copying it.
    let obs_id = format!("agent_memory:v1:{cand_id}-obs-1");
    for r in &mut records {
        if r.id() == obs_id
            && let GraphRecord::Node { evidence_links, .. } = r
        {
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v1:rust:symbol:cli_test".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "0.85".to_owned(),
                as_of_commit: Some("abcdef123456".to_owned()),
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
    }

    // Also add the codegraph target so validation passes
    let target_node = GraphRecord::node(
        "codegraph:v1:rust:symbol:cli_test".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("cli_test".to_owned()),
        "mock symbol".to_owned(),
    )
    .with_temporal(aletheia_egregore::ir::TemporalMetadata {
        git_commit: "abcdef123456".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-06-01T12:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-06-01T12:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(cand);
    graph.push(target_node);
    for r in records {
        graph.push(r);
    }
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "approved",
            "--graph",
            graph_path.to_str().unwrap(),
            "--data-dir",
            db_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&db_path).unwrap();
    let db_records = sink.read_all_records().unwrap();

    // Verify the observation node was copied
    assert!(db_records.iter().any(|r| r.id() == obs_id));

    // Verify the synthesized OBSERVES edge exists and carries the correct metadata
    let expected_edge_id = agent_memory_stable_id(&[
        "edge",
        "OBSERVES",
        &obs_id,
        "codegraph:v1:rust:symbol:cli_test",
    ]);
    let db_edge = db_records
        .iter()
        .find(|r| r.id() == expected_edge_id)
        .expect("Synthesized agent memory edge not found in database");

    if let GraphRecord::Edge {
        label,
        source,
        target,
        confidence,
        temporal,
        ..
    } = db_edge
    {
        assert_eq!(label, &EdgeLabel::Observes);
        assert_eq!(source, &obs_id);
        assert_eq!(target, "codegraph:v1:rust:symbol:cli_test");
        assert_eq!(confidence.as_deref(), Some("0.85"));
        let temp_meta = temporal
            .as_ref()
            .expect("Missing temporal metadata on synthesized edge");
        assert_eq!(temp_meta.git_commit, "abcdef123456");
    } else {
        panic!("Expected an Edge record");
    }
}

#[test]
fn test_cli_decide_fails_on_conflicting_copied_evidence_edges() {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("graph.jsonl");
    let db_path = temp.path().join("store");

    let cand_id = user_context_stable_id(&["candidate", "agent_memory_conflict_cand"]);
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
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = cand
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("MyPreferenceRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }
    let mut records = Vec::new();
    make_candidate_valid(&mut cand, &mut records);

    // Customize one of the generated observations to have conflicting evidence links (e.g. same target and relation but different as_of_commit)
    let obs_id = format!("agent_memory:v1:{cand_id}-obs-1");
    for r in &mut records {
        if r.id() == obs_id
            && let GraphRecord::Node { evidence_links, .. } = r
        {
            *evidence_links = Some(vec![
                EvidenceLink {
                    target_record_id: Some("codegraph:v1:rust:symbol:cli_test".to_owned()),
                    target_domain: "codegraph".to_owned(),
                    relation: "OBSERVES".to_owned(),
                    confidence: "0.85".to_owned(),
                    as_of_commit: Some("abcdef123456".to_owned()),
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some("codegraph:v1:rust:symbol:cli_test".to_owned()),
                    target_domain: "codegraph".to_owned(),
                    relation: "OBSERVES".to_owned(),
                    confidence: "0.85".to_owned(),
                    as_of_commit: Some("different_commit".to_owned()),
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]);
        }
    }

    // Also add the codegraph targets so validation passes
    let target_node_1 = GraphRecord::node(
        "codegraph:v1:rust:symbol:cli_test".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("cli_test".to_owned()),
        "mock symbol".to_owned(),
    )
    .with_temporal(aletheia_egregore::ir::TemporalMetadata {
        git_commit: "abcdef123456".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-06-01T12:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-06-01T12:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let target_node_2 = GraphRecord::node(
        "codegraph:v1:rust:symbol:cli_test".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("cli_test".to_owned()),
        "mock symbol".to_owned(),
    )
    .with_temporal(aletheia_egregore::ir::TemporalMetadata {
        git_commit: "different_commit".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-06-01T12:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-06-01T12:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(cand);
    graph.push(target_node_1);
    graph.push(target_node_2);
    for r in records {
        graph.push(r);
    }
    fs::write(&graph_path, graph.to_jsonl().unwrap()).unwrap();

    let output = egregore()
        .args([
            "decide",
            &cand_id,
            "--outcome",
            "approved",
            "--graph",
            graph_path.to_str().unwrap(),
            "--data-dir",
            db_path.to_str().unwrap(),
        ])
        .assert()
        .failure();

    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("conflicting as_of_commit values"));
}

#[test]
fn test_audit_trail_fails_for_invalid_contradicting_evidence_links() {
    use aletheia_egregore::query::audit_trail;

    let cand_id = "test_invalid_contra_cand";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate with bad contradicting link".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut user_context,
        ref mut valid_time,
        ref mut valid_time_source,
        ..
    } = candidate
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some("2026-06-01T12:00:00Z".to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            proposed_rule_text: Some("MyPreferenceRule".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            ..UserContextFields::empty()
        };
    }
    let dec_id = "dec_invalid_contra";
    let prompt_id = "prompt_invalid_contra";
    let pref_id = "target_pref_audit";

    let mut dec = GraphRecord::node(
        dec_id.to_owned(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        "Decision".to_owned(),
    );
    make_decision_valid(&mut dec, cand_id, prompt_id, pref_id);

    let mut prompt = GraphRecord::node(
        prompt_id.to_owned(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        "Prompt".to_owned(),
    );
    make_prompt_valid(&mut prompt, cand_id);

    let mut pref = GraphRecord::node(
        pref_id.to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Preference".to_owned(),
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
        *user_context = UserContextFields {
            rule_text: Some("RuleText".to_owned()),
            proposed_rule_kind: Some("preference".to_owned()),
            scope: Some(UserContextScope::default()),
            approval_decision_id: Some(dec_id.to_owned()),
            active_from: Some("2026-06-01T12:00:00Z".to_owned()),
            ..UserContextFields::empty()
        };
    }

    let mut records = vec![dec, prompt, pref];
    make_candidate_valid(&mut candidate, &mut records);

    // Set invalid contradicting link (e.g. wrong relation)
    if let GraphRecord::Node { user_context, .. } = &mut candidate {
        user_context.contradicting_evidence = Some(vec![EvidenceLink {
            target_record_id: Some("target_pref".to_owned()),
            target_domain: "user_context".to_owned(),
            relation: "INVALID_RELATION".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    records.push(candidate);

    // Call audit_trail and expect Err
    let target_policy = GraphRecord::node(
        "target_pref".to_owned(),
        NodeKind::Preference,
        None,
        None,
        None,
        "Target Preference".to_owned(),
    );
    let mut records_with_target = records.clone();
    records_with_target.push(target_policy);

    let durable_node = records_with_target
        .iter()
        .find(|r| {
            matches!(r, GraphRecord::Node { kind: NodeKind::Preference, .. } if r.id() != "target_pref")
        })
        .cloned()
        .unwrap();

    let result = audit_trail(&records_with_target, &durable_node);
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.contains("relation CONTRADICTS"));
}
