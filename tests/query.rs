#![allow(missing_docs)]

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, TemporalMetadata,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        verification_stable_id,
    },
    query::{largest_semantic_drifts, symbol_at_commit, symbol_context},
};

#[test]
fn symbol_at_commit_returns_the_temporal_symbol_record() {
    let first = GraphRecord::node(
        "symbol:answer:first".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "Rust function answer returns the old parser path".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let second = GraphRecord::node(
        "symbol:answer:second".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "Rust function answer returns the semantic history path".to_owned(),
    )
    .with_temporal(temporal("bbbbbbbb", "2026-01-02T00:00:00Z"));

    let records = [first, second];
    let found = symbol_at_commit(&records, "answer", "bbbbbbbb")
        .expect("symbol should be present at commit");

    assert_eq!(found.id(), "symbol:answer:second");
}

#[test]
fn largest_semantic_drifts_rank_drift_nodes_by_score() {
    let small = drift("drift:small", "answer", "aaaaaaaa", "bbbbbbbb", 0.25);
    let large = drift("drift:large", "answer", "bbbbbbbb", "cccccccc", 0.9);
    let edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::DriftsFrom,
        "drift:large".to_owned(),
        "symbol:answer".to_owned(),
        Some("1.0".to_owned()),
        "drift edge".to_owned(),
    );

    let records = [small, edge, large];
    let ranked = largest_semantic_drifts(&records, 1);

    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id(), "drift:large");
}

fn drift(id: &str, name: &str, before: &str, after: &str, score: f64) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some(name.to_owned()),
        format!("Semantic drift for {name}"),
    )
    .with_temporal(temporal(after, "2026-01-02T00:00:00Z"))
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "fake-code-model".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: format!("symbol:{name}"),
        prior_record_id: format!("symbol:{name}"),
        before_git_commit: before.to_owned(),
        after_git_commit: after.to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    })
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    }
}

const fn span(start_line: usize, end_line: usize) -> aletheia_egregore::SourceSpan {
    aletheia_egregore::SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line,
        end_line,
    }
}

// ── symbol_context tests ──────────────────────────────────────────────────────

/// Build a minimal Symbol node for context tests.
fn ctx_symbol(id: &str, name: &str, path: &str, line: usize) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "fn",
        path.to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 50,
            start_line: line,
            end_line: line + 5,
        },
        name.to_owned(),
        format!("Rust fn {name} at {path}:{line}"),
    )
}

/// Build an Observation node with an evidence link pointing at `target_id`.
fn ctx_observation(
    id: &str,
    text: &str,
    target_id: &str,
    relation: &str,
    confidence: &str,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        agent_memory_stable_id(&["obs", id]),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation: {text}"),
    );
    // Patch the fields not reachable via the public builder for these tests.
    if let GraphRecord::Node {
        id: ref mut record_id,
        text: ref mut text_field,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        confidence: ref mut confidence_field,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = record
    {
        *record_id = agent_memory_stable_id(&["obs", id]);
        *text_field = Some(text.to_owned());
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence_field = Some(confidence.to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(target_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: relation.to_owned(),
            confidence: confidence.to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    record
}

/// Build an Observation node with evidence link pointing at a non-existent target.
fn ctx_observation_unresolved(id: &str, text: &str, missing_target: &str) -> GraphRecord {
    ctx_observation(id, text, missing_target, "MENTIONS_SYMBOL", "0.8")
}

/// Build a Task node linked to a symbol.
fn ctx_task(id: &str, title: &str, symbol_id: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        aletheia_egregore::ir::project_stable_id(&["task", id]),
        NodeKind::Task,
        None,
        None,
        Some(title.to_owned()),
        format!("Task: {title}"),
    );
    if let GraphRecord::Node {
        title: ref mut title_field,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = record
    {
        *title_field = Some(title.to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(symbol_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    record
}

/// Build a Verification node linked to a symbol via `VALIDATED_BY`.
fn ctx_verification(id: &str, symbol_id: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        verification_stable_id(&["verification", id]),
        NodeKind::Verification,
        None,
        None,
        None,
        format!("Verification {id}"),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ref mut status,
        ref mut verification_kind,
        ..
    } = record
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("passed".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(symbol_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    record
}

// ── AC1: query returns all linked context ────────────────────────────────────

/// A fixture with one symbol, one observation, one task, one verification.
fn seeded_context_fixture() -> (Vec<GraphRecord>, String) {
    let sym_id = "codegraph:v4:aaaa0000symbol";
    let sym = ctx_symbol(sym_id, "compute_answer", "src/lib.rs", 10);
    let obs = ctx_observation(
        "obs1",
        "compute_answer needs doc",
        sym_id,
        "MENTIONS_SYMBOL",
        "0.9",
    );
    let task = ctx_task("task1", "Document compute_answer", sym_id);
    let verification = ctx_verification("ver1", sym_id);
    let records = vec![sym, obs, task, verification];
    (records, sym_id.to_owned())
}

#[test]
fn symbol_context_source_facts_include_symbol_node() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    assert!(
        !ctx.source_facts.is_empty(),
        "source_facts must include the symbol node"
    );
    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == "codegraph:v4:aaaa0000symbol"),
        "source_facts must contain the known symbol record ID"
    );
}

#[test]
fn symbol_context_observations_are_separated_from_source_facts() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    // Observations must appear in observations, not in source_facts
    assert!(
        !ctx.observations.is_empty(),
        "observations section must not be empty"
    );
    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|r| r.id()).collect();
    let fact_ids: std::collections::BTreeSet<&str> =
        ctx.source_facts.iter().map(|r| r.id()).collect();
    assert!(
        obs_ids.is_disjoint(&fact_ids),
        "observations and source_facts must be disjoint sets"
    );
}

#[test]
fn symbol_context_project_state_is_separated() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    assert!(
        !ctx.project_state.is_empty(),
        "project_state must include the task node"
    );
    let task_ids: std::collections::BTreeSet<&str> =
        ctx.project_state.iter().map(|r| r.id()).collect();
    let fact_ids: std::collections::BTreeSet<&str> =
        ctx.source_facts.iter().map(|r| r.id()).collect();
    assert!(
        task_ids.is_disjoint(&fact_ids),
        "project_state and source_facts must be disjoint"
    );
}

#[test]
fn symbol_context_verification_evidence_is_separated() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    assert!(
        !ctx.verification_evidence.is_empty(),
        "verification_evidence must include the verification node"
    );
    let ver_ids: std::collections::BTreeSet<&str> =
        ctx.verification_evidence.iter().map(|r| r.id()).collect();
    let fact_ids: std::collections::BTreeSet<&str> =
        ctx.source_facts.iter().map(|r| r.id()).collect();
    assert!(
        ver_ids.is_disjoint(&fact_ids),
        "verification_evidence and source_facts must be disjoint"
    );
}

// ── AC5: unresolved evidence surfaced, not silently dropped ──────────────────

#[test]
fn symbol_context_surfaces_unresolved_evidence_links() {
    let sym_id = "codegraph:v4:aaaa0001symbol";
    let sym = ctx_symbol(sym_id, "parse_input", "src/parser.rs", 5);
    let obs = ctx_observation_unresolved("obs_missing", "parse_input is slow", "missing:record:id");
    // The observation itself mentions a missing target; it should still appear in observations
    // and the missing reference should appear in unresolved.
    // The observation links to a missing target, but it's not linked to our symbol.
    // Use a second fixture that links to our symbol AND has a missing secondary evidence link:
    drop((sym, obs));
    let sym_id2 = "codegraph:v4:aaaa0002symbol";
    let sym2 = ctx_symbol(sym_id2, "serialize_output", "src/ser.rs", 20);
    let mut obs_mixed = ctx_observation(
        "obs_mixed",
        "serialize_output depends on missing",
        sym_id2,
        "MENTIONS_SYMBOL",
        "0.7",
    );
    // add a second evidence link to a missing target
    if let GraphRecord::Node {
        evidence_links: Some(ref mut links),
        ..
    } = obs_mixed
    {
        links.push(EvidenceLink {
            target_record_id: Some("missing:target:xyz".to_owned()),
            target_domain: "verification".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "0.5".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        });
    }
    let records2 = vec![sym2, obs_mixed];
    let ctx2 = symbol_context(&records2, "serialize_output");
    assert!(
        !ctx2.unresolved.is_empty(),
        "unresolved must contain the missing evidence link target"
    );
    let found = ctx2
        .unresolved
        .iter()
        .any(|u| u.target_handle.contains("missing:target:xyz"));
    assert!(
        found,
        "unresolved must surface the missing:target:xyz handle"
    );
}

// ── AC6: no-match is explicit and machine-readable ───────────────────────────

#[test]
fn symbol_context_no_match_returns_empty_context_with_is_no_match() {
    let sym = ctx_symbol("codegraph:v4:only_sym", "other_fn", "src/lib.rs", 1);
    let records = vec![sym];
    let ctx = symbol_context(&records, "nonexistent_function_xyz");
    assert!(
        ctx.is_no_match(),
        "must report is_no_match() for unknown symbol"
    );
    assert!(ctx.source_facts.is_empty());
    assert!(ctx.observations.is_empty());
    assert!(ctx.project_state.is_empty());
    assert!(ctx.artifacts.is_empty());
    assert!(ctx.verification_evidence.is_empty());
    assert!(ctx.unresolved.is_empty());
}

// ── edge-linked node evidence scan ───────────────────────────────────────────

#[test]
fn symbol_context_edge_linked_node_backing_evidence_is_classified() {
    // Obs --edge(MENTIONS_SYMBOL)--> Symbol
    // Obs --evidence_link(VALIDATED_BY)--> Ver
    // Symbol has no direct link to Ver.
    // Ver must still appear in verification_evidence via the post-processing pass.
    let sym_id = "codegraph:v4:edge_ev_sym001";
    let sym = GraphRecord::node(
        sym_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("edge_backed_fn".to_owned()),
        "fn edge_backed_fn".to_owned(),
    );

    let ver_id = verification_stable_id(&["ver", "edge_backed_ver1"]);
    let ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification backing edge_backed_fn obs".to_owned(),
    );

    let obs_id = agent_memory_stable_id(&["obs", "edge_backed_obs1"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about edge_backed_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(ver_id.clone()),
            target_domain: "verification".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // The link from Obs to Symbol is an EDGE record, not an evidence_link.
    let edge = GraphRecord::Edge {
        id: "edge:mentions_symbol:edge_backed".to_owned(),
        label: EdgeLabel::MentionsSymbol,
        source: obs_id,
        target: sym_id.to_owned(),
        schema_version: 0,
        confidence: None,
        temporal: None,
        summary: "obs mentions edge_backed_fn".to_owned(),
        producer: None,
    };

    let records = vec![sym, ver, obs, edge];
    let ctx = symbol_context(&records, "edge_backed_fn");

    assert!(
        !ctx.observations.is_empty(),
        "observation must be in observations (connected via edge)"
    );
    assert!(
        !ctx.verification_evidence.is_empty(),
        "backing verification must appear via post-processing of edge-classified obs"
    );
    let ver_in_evidence = ctx
        .verification_evidence
        .iter()
        .any(|r| r.id() == ver_id.as_str());
    assert!(
        ver_in_evidence,
        "the specific verification record must be in verification_evidence"
    );
    assert!(
        ctx.unresolved.is_empty(),
        "no unresolved — all targets are present"
    );
}

// ── tombstone: deleted symbols yield no-match, not stale context ─────────────

#[test]
fn symbol_context_tombstoned_symbol_returns_no_match() {
    let sym_id = "codegraph:v4:deadfn0001symbol".to_owned();
    let sym = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("dead_fn".to_owned()),
        "fn dead_fn was deleted".to_owned(),
    );
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:codegraph:v4:deadfn0001".to_owned(),
        schema_version: 0,
        deleted_id: sym_id,
        summary: "symbol removed".to_owned(),
        producer: None,
    };

    let records = vec![sym, tombstone];
    let ctx = symbol_context(&records, "dead_fn");
    assert!(
        ctx.is_no_match(),
        "tombstoned symbol must yield no-match, not stale context"
    );
}

// ── backed evidence: present non-symbol evidence link targets are classified ──

#[test]
fn symbol_context_classifies_present_backing_evidence_of_linked_observation() {
    // Observation O links to Symbol S (MENTIONS_SYMBOL) and also to
    // Verification V (VALIDATED_BY). V has no direct link to S.
    // The query must still surface V in verification_evidence because it
    // backs an observation that is already in the response.
    let sym_id = agent_memory_stable_id(&["sym", "parse_input_backed"]);
    let sym = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/parser.rs".to_owned()),
        None,
        Some("parse_input".to_owned()),
        "fn parse_input".to_owned(),
    );

    let ver_id = verification_stable_id(&["ver", "backed_ver1"]);
    let ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification of parse_input test run".to_owned(),
    );

    let obs_id = agent_memory_stable_id(&["obs", "backed_obs1"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "parse_input is safe".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            EvidenceLink {
                target_record_id: Some(sym_id),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
            EvidenceLink {
                target_record_id: Some(ver_id.clone()),
                target_domain: "verification".to_owned(),
                relation: "VALIDATED_BY".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
        ]);
    }

    let records = vec![sym, ver, obs];
    let ctx = symbol_context(&records, "parse_input");

    assert!(
        !ctx.observations.is_empty(),
        "observation must appear in observations"
    );
    assert!(
        !ctx.verification_evidence.is_empty(),
        "backing verification must appear in verification_evidence even without a direct symbol link"
    );
    let ver_in_evidence = ctx
        .verification_evidence
        .iter()
        .any(|r| r.id() == ver_id.as_str());
    assert!(
        ver_in_evidence,
        "the specific verification record must be in verification_evidence"
    );
    assert!(
        ctx.unresolved.is_empty(),
        "no unresolved — both targets are present in the store"
    );
}

// ── AC7: stable output ordering ──────────────────────────────────────────────

#[test]
fn symbol_context_ordering_is_stable_for_repeated_identical_queries() {
    let (records, _) = seeded_context_fixture();

    let ctx_a = symbol_context(&records, "compute_answer");
    let ctx_b = symbol_context(&records, "compute_answer");

    let ids_a: Vec<&str> = ctx_a.source_facts.iter().map(|r| r.id()).collect();
    let ids_b: Vec<&str> = ctx_b.source_facts.iter().map(|r| r.id()).collect();
    assert_eq!(
        ids_a, ids_b,
        "source_facts ordering must be identical across repeated calls"
    );

    let obs_ids_a: Vec<&str> = ctx_a.observations.iter().map(|r| r.id()).collect();
    let obs_ids_b: Vec<&str> = ctx_b.observations.iter().map(|r| r.id()).collect();
    assert_eq!(
        obs_ids_a, obs_ids_b,
        "observations ordering must be identical across repeated calls"
    );
}

// ── AC2: every source fact has record_id plus path/span or commit ─────────────

#[test]
fn symbol_context_source_facts_have_record_id_and_path_or_commit() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    for fact in &ctx.source_facts {
        assert!(
            !fact.id().is_empty(),
            "every source_fact must have a non-empty record_id"
        );
        let has_path = matches!(
            fact,
            GraphRecord::Node {
                repo_relative_path: Some(_),
                ..
            }
        );
        let has_commit = matches!(
            fact,
            GraphRecord::Node {
                temporal: Some(_),
                ..
            }
        );
        let has_valid_time = matches!(
            fact,
            GraphRecord::Node {
                valid_time: Some(_),
                ..
            }
        );
        assert!(
            has_path || has_commit || has_valid_time,
            "source_fact {} must carry a repo_relative_path, temporal commit, or valid_time",
            fact.id()
        );
    }
}

// ── AC3: every observation has provenance fields ─────────────────────────────

#[test]
fn symbol_context_observations_carry_provenance_fields() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    for obs in &ctx.observations {
        assert!(
            !obs.id().is_empty(),
            "every observation must have a record_id"
        );
        let has_provenance = matches!(
            obs,
            GraphRecord::Node {
                agent_id: Some(_),
                observed_at: Some(_),
                confidence: Some(_),
                ..
            }
        );
        assert!(
            has_provenance,
            "observation {} must carry agent_id, observed_at, and confidence",
            obs.id()
        );
    }
}

// ── AC4: observation is never presented as source truth ──────────────────────

#[test]
fn symbol_context_observation_node_kind_is_never_in_source_facts() {
    let (records, _) = seeded_context_fixture();
    let ctx = symbol_context(&records, "compute_answer");
    for fact in &ctx.source_facts {
        let is_observation = matches!(
            fact,
            GraphRecord::Node {
                kind: NodeKind::Observation,
                ..
            }
        );
        assert!(
            !is_observation,
            "Observation node {} must never appear in source_facts",
            fact.id()
        );
    }
}

// ── 2-hop BFS: AcceptanceCriterion discovered via Task → AC edge ─────────────

#[test]
fn symbol_context_two_hop_bfs_discovers_acceptance_criteria_via_task() {
    // Hop 0 (seed): Symbol S
    // Hop 1: Task T  — evidence_link MENTIONS_SYMBOL → S
    // Hop 2: AC      — OWNED_BY_TASK edge: T → AC
    //
    // Without 2-hop BFS the AC would be invisible because it has no direct link to S.
    let sym_id = "codegraph:v4:bfs_two_hop_sym01";
    let sym = ctx_symbol(sym_id, "bfs_two_hop_fn", "src/bfs.rs", 1);

    // Build a Task with evidence_link pointing at the symbol (discovered in hop 1).
    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "bfs_two_hop_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Two-hop BFS task".to_owned()),
        "Task: Two-hop BFS task".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ref mut title,
        ..
    } = task
    {
        *title = Some("Two-hop BFS task".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Build an AcceptanceCriterion with no direct link to the symbol.
    let ac_id = aletheia_egregore::ir::project_stable_id(&["ac", "bfs_two_hop_ac"]);
    let ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC: bfs_two_hop_fn must be documented".to_owned()),
        "AC: bfs_two_hop_fn must be documented".to_owned(),
    );

    // OWNED_BY_TASK edge: Task → AcceptanceCriterion (discovered in hop 2).
    let edge = GraphRecord::edge(
        EdgeLabel::OwnedByTask,
        task_id.clone(),
        ac_id.clone(),
        None,
        "task owns AC".to_owned(),
    );

    let records = vec![sym, task, ac, edge];
    let ctx = symbol_context(&records, "bfs_two_hop_fn");

    // The symbol must be in source_facts.
    assert!(
        ctx.source_facts.iter().any(|r| r.id() == sym_id),
        "symbol must be in source_facts"
    );
    // The task must be in project_state (hop 1).
    assert!(
        ctx.project_state.iter().any(|r| r.id() == task_id),
        "task must be in project_state (hop 1)"
    );
    // The AC must be in project_state (hop 2).
    assert!(
        ctx.project_state.iter().any(|r| r.id() == ac_id),
        "acceptance criterion must be in project_state via 2-hop BFS (hop 2)"
    );
}

// ── Edges-based linking (MENTIONS_SYMBOL/OBSERVES edge, not just evidence_links) ─

#[test]
fn symbol_context_finds_observations_linked_via_edges() {
    let sym_id = "codegraph:v4:edge_test_sym";
    let sym = ctx_symbol(sym_id, "edge_linked_fn", "src/edge.rs", 5);
    let obs_id = agent_memory_stable_id(&["obs", "edge_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "edge-linked observation".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut schema_version,
        ..
    } = obs
    {
        *text = Some("edge_linked_fn analysis".to_owned());
        *agent_id = Some("agent:edge".to_owned());
        *session_id = Some("session:edge".to_owned());
        *observed_at = Some("2026-02-01T00:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    // Link via MENTIONS_SYMBOL edge: obs → sym
    let edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "observation mentions symbol".to_owned(),
    );
    let records = vec![sym, obs, edge];
    let ctx = symbol_context(&records, "edge_linked_fn");
    assert!(
        !ctx.observations.is_empty(),
        "observations found via MENTIONS_SYMBOL edge must appear in observations section"
    );
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id),
        "the edge-linked observation must be in the observations section"
    );
}

// ── Finding: tombstoned context records must be excluded ─────────────────────

#[test]
fn symbol_context_excludes_tombstoned_context_records() {
    // Observation O is linked to Symbol S but then tombstoned.
    // It must NOT appear in the context output.
    let sym_id = "codegraph:v4:tomb_ctx_sym001";
    let sym = ctx_symbol(sym_id, "tomb_ctx_fn", "src/lib.rs", 1);

    // Use the raw key (not the pre-hashed ID) so ctx_observation hashes once.
    let obs = ctx_observation(
        "tomb_ctx_obs1",
        "should be hidden",
        sym_id,
        "MENTIONS_SYMBOL",
        "1.0",
    );
    let obs_id = obs.id().to_owned();
    let obs_tombstone = GraphRecord::Tombstone {
        id: "tombstone:obs_ctx:001".to_owned(),
        schema_version: 0,
        deleted_id: obs_id.clone(),
        summary: "observation removed".to_owned(),
        producer: None,
    };

    let records = vec![sym, obs, obs_tombstone];
    let ctx = symbol_context(&records, "tomb_ctx_fn");

    assert!(
        !ctx.is_no_match(),
        "symbol itself is not tombstoned so context must not be no_match"
    );
    assert!(
        ctx.observations.iter().all(|r| r.id() != obs_id.as_str()),
        "tombstoned observation must not appear in context"
    );
}

// ── Finding: sibling codegraph symbols must not pollute source_facts ─────────

#[test]
fn symbol_context_sibling_symbol_not_pulled_via_observation() {
    // Observation O has two evidence links: MENTIONS_SYMBOL → foo AND → bar.
    // Querying for foo must NOT pull bar into source_facts via the 2nd hop.
    let foo_id = "codegraph:v4:sibling_foo_sym";
    let foo = ctx_symbol(foo_id, "sibling_foo", "src/foo.rs", 1);

    let bar_id = "codegraph:v4:sibling_bar_sym";
    let bar = ctx_symbol(bar_id, "sibling_bar", "src/bar.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "sibling_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "sibling observation".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_links = Some(vec![
            EvidenceLink {
                target_record_id: Some(foo_id.to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
            EvidenceLink {
                target_record_id: Some(bar_id.to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.8".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
        ]);
    }

    let records = vec![foo, bar, obs];
    let ctx = symbol_context(&records, "sibling_foo");

    // The sibling symbol must NOT appear in source_facts.
    assert!(
        ctx.source_facts.iter().all(|r| r.id() != bar_id),
        "sibling symbol bar must not appear in source_facts when querying foo"
    );
    // The observation must still appear (it references foo).
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "observation must still appear in observations"
    );
}

// ── Finding: temporal (scan-history) symbols survive later tombstones ─────────

#[test]
fn symbol_context_temporal_symbol_survives_later_tombstone() {
    // In a scan-history graph a symbol record carries temporal metadata.
    // A tombstone for the same stable ID means the symbol was deleted in the
    // current state, but the historical snapshot should still be queryable.
    let sym_id = "codegraph:v4:temporal_sym_hist001";
    let sym = ctx_symbol(sym_id, "hist_fn", "src/hist.rs", 1).with_temporal(TemporalMetadata {
        git_commit: "aabbccdd".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal:001".to_owned(),
        schema_version: 0,
        deleted_id: sym_id.to_owned(),
        summary: "symbol removed in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym, tombstone];
    let ctx = symbol_context(&records, "hist_fn");

    assert!(
        !ctx.is_no_match(),
        "temporal symbol must remain visible even when a tombstone for its ID exists"
    );
    assert!(
        ctx.source_facts.iter().any(|r| r.id() == sym_id),
        "temporal symbol must appear in source_facts"
    );
}

// ── Finding: 3-hop BFS discovers Verification via Task → AC → Verification ───

#[test]
fn symbol_context_three_hop_bfs_discovers_verification_via_ac() {
    // Symbol S
    // Task T — evidence_link MENTIONS_SYMBOL → S          (hop 1)
    // AC    — OWNED_BY_TASK edge: T → AC                  (hop 2)
    // Ver   — CLOSES_ACCEPTANCE_CRITERION edge: Ver → AC  (hop 3)
    let sym_id = "codegraph:v4:three_hop_sym001";
    let sym = ctx_symbol(sym_id, "three_hop_fn", "src/three.rs", 1);

    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "three_hop_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Three-hop task".to_owned()),
        "Task: Three-hop task".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ref mut title,
        ..
    } = task
    {
        *title = Some("Three-hop task".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let ac_id = aletheia_egregore::ir::project_stable_id(&["ac", "three_hop_ac"]);
    let ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC: three_hop_fn works".to_owned()),
        "AC: three_hop_fn works".to_owned(),
    );

    let ver_id = verification_stable_id(&["ver", "three_hop_ver"]);
    let ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification closing the AC".to_owned(),
    );

    // Edges connecting the chain.
    let owned_edge = GraphRecord::edge(
        EdgeLabel::OwnedByTask,
        task_id.clone(),
        ac_id.clone(),
        None,
        "task owns AC".to_owned(),
    );
    // Schema direction: AC (source) --CLOSES_ACCEPTANCE_CRITERION--> Verification (target).
    // Forward traversal from AC (hop 2 frontier) discovers Ver (hop 3).
    let closes_edge = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_id.clone(),
        ver_id.clone(),
        None,
        "AC closed by verification".to_owned(),
    );

    let records = vec![sym, task, ac, ver, owned_edge, closes_edge];
    let ctx = symbol_context(&records, "three_hop_fn");

    assert!(
        ctx.project_state.iter().any(|r| r.id() == task_id),
        "task must be in project_state (hop 1)"
    );
    assert!(
        ctx.project_state.iter().any(|r| r.id() == ac_id),
        "AC must be in project_state (hop 2)"
    );
    assert!(
        ctx.verification_evidence.iter().any(|r| r.id() == ver_id),
        "verification must be in verification_evidence via 3-hop BFS (hop 3)"
    );
}

// ── Finding: shared evidence sink must not fan out to sibling observations ────

#[allow(clippy::too_many_lines)]
#[test]
fn symbol_context_shared_validation_run_does_not_pull_sibling_observations() {
    // ObsA --MENTIONS_SYMBOL(edge)--> SymA  (hop 1: ObsA discovered)
    // ObsA --VALIDATED_BY(edge)----> Run    (hop 2: Run discovered)
    // ObsB --VALIDATED_BY(edge)----> Run    (ObsB shares the run but has no link to SymA)
    //
    // Without directional guard, hop 3 would traverse Run backward via
    // VALIDATED_BY and classify ObsB — even though ObsB has no connection to SymA.
    let sym_id = "codegraph:v4:sharedrun_sym001";
    let sym = ctx_symbol(sym_id, "sharedrun_fn", "src/lib.rs", 1);

    let linked_obs_id = agent_memory_stable_id(&["obs", "sharedrun_obs_a"]);
    let mut linked_obs = GraphRecord::node(
        linked_obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation linked to sharedrun_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = linked_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    let sibling_obs_id = agent_memory_stable_id(&["obs", "sharedrun_obs_b"]);
    let mut sibling_obs = GraphRecord::node(
        sibling_obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "sibling observation — unrelated to sharedrun_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = sibling_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.8".to_owned());
    }

    let run_id = verification_stable_id(&["run", "sharedrun_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run shared by two observations".to_owned(),
    );

    // linked_obs --MENTIONS_SYMBOL--> SymA (edge connecting the observation to the symbol)
    let sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        linked_obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "linked_obs mentions sharedrun_fn".to_owned(),
    );
    // linked_obs --VALIDATED_BY--> Run (edge: linked_obs is validated by the shared run)
    let linked_run_edge = GraphRecord::edge(
        EdgeLabel::ValidatedBy,
        linked_obs_id.clone(),
        run_id.clone(),
        None,
        "linked_obs validated by run".to_owned(),
    );
    // sibling_obs --VALIDATED_BY--> Run (edge: sibling_obs also uses the same run, unrelated to SymA)
    let sibling_run_edge = GraphRecord::edge(
        EdgeLabel::ValidatedBy,
        sibling_obs_id.clone(),
        run_id.clone(),
        None,
        "sibling_obs validated by same run".to_owned(),
    );

    let records = vec![
        sym,
        linked_obs,
        sibling_obs,
        run,
        sym_edge,
        linked_run_edge,
        sibling_run_edge,
    ];
    let ctx = symbol_context(&records, "sharedrun_fn");

    assert!(
        ctx.observations
            .iter()
            .any(|r| r.id() == linked_obs_id.as_str()),
        "linked_obs must be in observations (linked to symbol)"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "run must be in verification_evidence (backs linked_obs)"
    );
    assert!(
        ctx.observations
            .iter()
            .all(|r| r.id() != sibling_obs_id.as_str()),
        "sibling_obs must NOT appear — it only shares the run, not the symbol link"
    );
}

// ── Finding: FAILED_ON traversal must work from Symbol seed ──────────────────

#[test]
fn symbol_context_failed_on_edge_traverses_from_symbol_seed() {
    // Failure --FAILED_ON--> Symbol is the documented edge direction.
    // The symbol is the target; querying it must discover the Failure node
    // via backward traversal (frontier contains target → classify source).
    let sym_id = "codegraph:v4:failed_on_sym001";
    let sym = ctx_symbol(sym_id, "failing_fn", "src/lib.rs", 1);

    let fail_id = agent_memory_stable_id(&["fail", "failed_on_fail1"]);
    let mut fail = GraphRecord::node(
        fail_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "failure on failing_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = fail
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    // Failure --FAILED_ON--> Symbol (failure is the SOURCE, symbol is the TARGET)
    let edge = GraphRecord::edge(
        EdgeLabel::FailedOn,
        fail_id.clone(),
        sym_id.to_owned(),
        None,
        "failure failed_on sym".to_owned(),
    );

    let records = vec![sym, fail, edge];
    let ctx = symbol_context(&records, "failing_fn");

    assert!(
        ctx.observations.iter().any(|r| r.id() == fail_id.as_str()),
        "Failure node must be discovered via backward FAILED_ON traversal from symbol seed"
    );
}

// ── Finding: tombstoned nodes must not expand the BFS frontier ────────────────

#[test]
fn symbol_context_tombstoned_context_node_does_not_expand_frontier() {
    // Observation O is tombstoned. It has a VALIDATED_BY edge to Verification V.
    // O is linked to Symbol S via edge.
    // V must NOT appear in context because O is tombstoned and must not expand
    // the frontier to expose its backing verification.
    let sym_id = "codegraph:v4:tomb_expand_sym001";
    let sym = ctx_symbol(sym_id, "tomb_expand_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "tomb_expand_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "tombstoned observation".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    let ver_id = verification_stable_id(&["ver", "tomb_expand_ver"]);
    let ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "verification behind tombstoned obs".to_owned(),
    );

    // O --MENTIONS_SYMBOL--> S (edge connecting obs to symbol)
    let obs_sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        None,
        "obs mentions sym".to_owned(),
    );
    // O --VALIDATED_BY--> V (edge: obs is backed by verification)
    let obs_ver_edge = GraphRecord::edge(
        EdgeLabel::ValidatedBy,
        obs_id.clone(),
        ver_id.clone(),
        None,
        "obs validated by ver".to_owned(),
    );
    // Tombstone for O
    let obs_tombstone = GraphRecord::Tombstone {
        id: "tombstone:tomb_expand_obs".to_owned(),
        schema_version: 0,
        deleted_id: obs_id.clone(),
        summary: "obs removed".to_owned(),
        producer: None,
    };

    let records = vec![sym, obs, ver, obs_sym_edge, obs_ver_edge, obs_tombstone];
    let ctx = symbol_context(&records, "tomb_expand_fn");

    assert!(
        !ctx.is_no_match(),
        "symbol is present; must not be no_match"
    );
    assert!(
        ctx.observations.iter().all(|r| r.id() != obs_id.as_str()),
        "tombstoned observation must not appear in context"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .all(|r| r.id() != ver_id.as_str()),
        "verification reachable only through tombstoned obs must not appear in context"
    );
}

// ── Finding: topology edges between seed nodes appear in topology_edges ───────

#[test]
fn symbol_context_topology_edges_include_defines_edge() {
    // A DEFINES edge from a co-located File to the Symbol must appear in
    // topology_edges so consumers can cite the file→symbol relationship.
    let sym_id = "codegraph:v4:topo_sym001";
    let sym = ctx_symbol(sym_id, "topo_fn", "src/topo.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "src/topo.rs"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/topo.rs".to_owned()),
        None,
        None,
        "src/topo.rs".to_owned(),
    );

    // DEFINES edge: File → Symbol
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        sym_id.to_owned(),
        None,
        "file defines symbol".to_owned(),
    );
    let defines_edge_id = defines_edge.id().to_owned();

    let records = vec![sym, file, defines_edge];
    let ctx = symbol_context(&records, "topo_fn");

    assert!(
        !ctx.source_facts.is_empty(),
        "symbol and file must be in source_facts"
    );
    assert!(
        !ctx.topology_edges.is_empty(),
        "DEFINES edge must appear in topology_edges"
    );
    assert!(
        ctx.topology_edges
            .iter()
            .any(|r| r.id() == defines_edge_id.as_str()),
        "the specific DEFINES edge must be in topology_edges"
    );
}

// ── Finding: missing edge endpoints must not expand BFS frontier ──────────────

#[test]
fn symbol_context_missing_edge_endpoint_does_not_expand_frontier() {
    // Edge: ghost_id --MENTIONS_SYMBOL--> Symbol (ghost_id is not in records)
    // Edge: Obs_Q --MENTIONS_SYMBOL--> ghost_id
    //
    // Without the fix: ghost_id is added to frontier even though classify_and_insert
    // returns early (by_id miss). In the next hop ghost_id is in frontier, causing
    // Obs_Q to be classified via backward MENTIONS_SYMBOL traversal.
    let sym_id = "codegraph:v4:ghost_ep_sym001";
    let sym = ctx_symbol(sym_id, "ghost_ep_fn", "src/lib.rs", 1);

    // Observation Q is legitimately present but only linked to the ghost endpoint.
    let unrelated_id = agent_memory_stable_id(&["obs", "ghost_ep_unrelated"]);
    let mut unrelated_obs = GraphRecord::node(
        unrelated_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "unrelated observation pointing at ghost".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = unrelated_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    let ghost_id = "ghost:missing:record";

    // Ghost --MENTIONS_SYMBOL--> Symbol (ghost is source, not present in records)
    let ghost_sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        ghost_id.to_owned(),
        sym_id.to_owned(),
        None,
        "ghost mentions sym".to_owned(),
    );
    // Unrelated --MENTIONS_SYMBOL--> Ghost (would be traversed from ghost if ghost is in frontier)
    let unrelated_ghost_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        unrelated_id.clone(),
        ghost_id.to_owned(),
        None,
        "unrelated obs points at ghost".to_owned(),
    );

    let records = vec![sym, unrelated_obs, ghost_sym_edge, unrelated_ghost_edge];
    let ctx = symbol_context(&records, "ghost_ep_fn");

    assert!(
        ctx.observations
            .iter()
            .all(|r| r.id() != unrelated_id.as_str()),
        "observation linked only via missing ghost endpoint must not appear in context"
    );
}

// ── Finding: skipped codegraph targets (siblings) must not expand frontier ────

#[test]
fn symbol_context_skipped_codegraph_target_does_not_expand_frontier() {
    // Obs O is linked to Symbol S via evidence_link (classified via node arm).
    // Edge O --MENTIONS_SYMBOL--> Symbol T (sibling, not in seed_ids).
    // Obs Q has evidence_link MENTIONS_SYMBOL --> T.
    //
    // Without the fix: T is discovered via O→T edge but skipped by classify_and_insert
    // (non-seed SourceFact). T is still pushed to frontier. In the next hop, Q is
    // discovered via T in the frontier and wrongly classified as context for S.
    let sym_s_id = "codegraph:v4:skipfrontier_sym_s";
    let sym_s = ctx_symbol(sym_s_id, "skipfrontier_fn_s", "src/s.rs", 1);

    let sibling_id = "codegraph:v4:skipfrontier_sym_t";
    let sibling_sym = ctx_symbol(sibling_id, "skipfrontier_fn_t", "src/t.rs", 1);

    // Obs O: linked to S via evidence_link (will be classified in hop 1)
    let obs_linked = ctx_observation(
        "skipfrontier_obs_o",
        "obs about S",
        sym_s_id,
        "MENTIONS_SYMBOL",
        "0.9",
    );
    let obs_linked_id = obs_linked.id().to_owned();

    // Obs Q: linked to T via evidence_link (must NOT appear when querying S)
    let obs_unrelated = ctx_observation(
        "skipfrontier_obs_q",
        "obs about T only",
        sibling_id,
        "MENTIONS_SYMBOL",
        "0.8",
    );
    let obs_unrelated_id = obs_unrelated.id().to_owned();

    // Edge O --MENTIONS_SYMBOL--> T (O points to sibling T; T should be skipped)
    let linked_sibling_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_linked_id.clone(),
        sibling_id.to_owned(),
        None,
        "obs_o also mentions sym_t".to_owned(),
    );

    let records = vec![
        sym_s,
        sibling_sym,
        obs_linked,
        obs_unrelated,
        linked_sibling_edge,
    ];
    let ctx = symbol_context(&records, "skipfrontier_fn_s");

    assert!(
        ctx.observations
            .iter()
            .any(|r| r.id() == obs_linked_id.as_str()),
        "obs_o must be in observations (linked to sym_s)"
    );
    assert!(
        ctx.observations
            .iter()
            .all(|r| r.id() != obs_unrelated_id.as_str()),
        "obs_q must NOT appear — it is only linked to sibling sym_t, not to sym_s"
    );
    assert!(
        ctx.source_facts.iter().all(|r| r.id() != sibling_id),
        "sibling sym_t must not appear in source_facts"
    );
}

// ── Finding: ClosesAcceptanceCriterion must be forward-only ───────────────────
//
// Schema direction: AC --CLOSES_ACCEPTANCE_CRITERION--> Verification.
// (AC is the source; Verification is the closing evidence target.)
//
// Making this forward-only prevents backward traversal from Verification
// to unrelated ACs that happen to share the same Verification run.

#[test]
fn symbol_context_closes_ac_is_forward_only() {
    // Symbol S
    // Obs --MENTIONS_SYMBOL edge--> S (hop 1)
    // Obs --VALIDATED_BY edge--> Run  (hop 2: Run discovered)
    // AC2 (source) --CLOSES_AC--> Run (target): unrelated AC that closes the same Run
    //
    // Without the forward-only guard, hop 3 traverses backward from Run through
    // CLOSES_ACCEPTANCE_CRITERION and classifies AC2 — even though AC2 has no link to S.
    let sym_id = "codegraph:v4:closes_ac_fwd_sym001";
    let sym = ctx_symbol(sym_id, "closes_ac_fwd_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "closes_ac_fwd_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation about closes_ac_fwd_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    let run_id = verification_stable_id(&["run", "closes_ac_fwd_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run shared with unrelated AC".to_owned(),
    );

    let ac2_id = aletheia_egregore::ir::project_stable_id(&["ac", "closes_ac_fwd_unrelated_ac"]);
    let ac2 = GraphRecord::node(
        ac2_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("Unrelated AC closed by same run".to_owned()),
        "Unrelated AC".to_owned(),
    );

    // Obs --MENTIONS_SYMBOL edge--> S
    let sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs mentions closes_ac_fwd_fn".to_owned(),
    );
    // Obs --VALIDATED_BY edge--> Run (forward-only: Obs → Run, not Run → Obs)
    let val_edge = GraphRecord::edge(
        EdgeLabel::ValidatedBy,
        obs_id.clone(),
        run_id.clone(),
        None,
        "obs validated by run".to_owned(),
    );
    // AC2 (source) --CLOSES_ACCEPTANCE_CRITERION--> Run (target)
    // Schema direction: AC is source, Verification is target.
    // With forward-only: when Run is in frontier (hop 2), backward traversal to AC2
    // is blocked, so AC2 must NOT appear in project_state.
    let closes_edge = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac2_id.clone(),
        run_id.clone(),
        None,
        "unrelated AC closes same run".to_owned(),
    );

    let records = vec![sym, obs, run, ac2, sym_edge, val_edge, closes_edge];
    let ctx = symbol_context(&records, "closes_ac_fwd_fn");

    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "obs must be in observations"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "run must be in verification_evidence"
    );
    assert!(
        ctx.project_state.iter().all(|r| r.id() != ac2_id.as_str()),
        "unrelated AC2 must NOT appear — it only shares the Run, not the symbol link"
    );
}

// ── Finding: backfill discoveries must expand the BFS frontier ────────────────
//
// Nodes classified via the edge arm have their evidence_links scanned in
// post-processing backfill. If backfill classifies a Task T (discovered via
// Obs's evidence_links), T's outgoing edges (e.g. OwnedByTask → AC) must be
// traversed so the AC appears in project_state.

#[test]
fn symbol_context_backfill_task_expands_to_ac() {
    // Symbol S
    // Obs --MENTIONS_SYMBOL edge--> S  (Obs classified via edge arm, hop 1)
    // Obs --evidence_link MENTIONS_SYMBOL--> T  (Task T classified via backfill)
    // Task T --OWNED_BY_TASK edge--> AC  (AC must be discovered via extra edge pass)
    let sym_id = "codegraph:v4:backfill_expand_sym001";
    let sym = ctx_symbol(sym_id, "backfill_expand_fn", "src/lib.rs", 1);

    // Task T linked to symbol via Obs evidence_link
    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "backfill_expand_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Backfill expand task".to_owned()),
        "Task: Backfill expand task".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ..
    } = task
    {
        *title = Some("Backfill expand task".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
    }

    // AC owned by Task, no direct link to symbol
    let ac_id = aletheia_egregore::ir::project_stable_id(&["ac", "backfill_expand_ac"]);
    let ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC from backfill expansion".to_owned()),
        "AC from backfill expansion".to_owned(),
    );

    // Obs: linked to symbol via edge AND has evidence_link to Task T
    let obs_id = agent_memory_stable_id(&["obs", "backfill_expand_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation about backfill_expand_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        // evidence_link to Task T (this is what backfill should discover)
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(task_id.clone()),
            target_domain: "project".to_owned(),
            relation: "REFERENCES_TASK".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Edge: Obs --MENTIONS_SYMBOL--> S (classifies Obs via edge arm)
    let obs_sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs mentions backfill_expand_fn".to_owned(),
    );

    // Edge: Task T --OWNED_BY_TASK--> AC (AC is discovered via extra edge pass on backfill results)
    let owned_edge = GraphRecord::edge(
        EdgeLabel::OwnedByTask,
        task_id.clone(),
        ac_id.clone(),
        None,
        "task owns AC".to_owned(),
    );

    let records = vec![sym, obs, task, ac, obs_sym_edge, owned_edge];
    let ctx = symbol_context(&records, "backfill_expand_fn");

    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "obs must be in observations (edge arm)"
    );
    assert!(
        ctx.project_state.iter().any(|r| r.id() == task_id.as_str()),
        "task must be in project_state (discovered via backfill from obs evidence_link)"
    );
    assert!(
        ctx.project_state.iter().any(|r| r.id() == ac_id.as_str()),
        "AC must be in project_state via extra edge pass on backfill-discovered task"
    );
}

// ── Finding: all temporal versions of a symbol must appear in source_facts ────
//
// When a scan-history graph contains multiple temporal records with the same
// stable ID (different commits), all versions must appear in source_facts.
// The BTreeMap by_id lookup (last-write-wins) previously silenced all but one.

#[test]
fn symbol_context_all_temporal_versions_appear_in_source_facts() {
    let sym_id = "codegraph:v4:multi_temporal_sym";
    let sym_v1 = ctx_symbol(sym_id, "multi_temporal_fn", "src/temporal.rs", 1).with_temporal(
        TemporalMetadata {
            git_commit: "aaaa1111".to_owned(),
            git_parent_commits: vec![],
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: None,
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        },
    );
    let sym_v2 = ctx_symbol(sym_id, "multi_temporal_fn", "src/temporal.rs", 1).with_temporal(
        TemporalMetadata {
            git_commit: "bbbb2222".to_owned(),
            git_parent_commits: vec![],
            valid_time: "2026-02-01T00:00:00Z".to_owned(),
            author_time: None,
            observed_at: "2026-02-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        },
    );

    let records = vec![sym_v1, sym_v2];
    let ctx = symbol_context(&records, "multi_temporal_fn");

    assert!(
        !ctx.is_no_match(),
        "multi-temporal symbol must not be no_match"
    );
    assert_eq!(
        ctx.source_facts.len(),
        2,
        "both temporal versions must appear in source_facts; got {}",
        ctx.source_facts.len()
    );
    let commits: Vec<&str> = ctx
        .source_facts
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node {
                temporal: Some(t), ..
            } = r
            {
                Some(t.git_commit.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        commits.contains(&"aaaa1111"),
        "v1 commit must be in source_facts"
    );
    assert!(
        commits.contains(&"bbbb2222"),
        "v2 commit must be in source_facts"
    );
}

// ── Finding: multi-repo file co-location must use DEFINES edges ───────────────
//
// Path-based co-location adds every File node sharing the same
// `repo_relative_path`, even from different repositories. When DEFINES edges
// are present they unambiguously identify the correct file, so they must be
// used as the primary mechanism. Only fall back to path-matching when no
// DEFINES edges are found.

#[test]
fn symbol_context_multi_repo_file_excluded_without_defines_edge() {
    // Symbol S at "src/lib.rs" (repo A)
    // File A at "src/lib.rs" with DEFINES edge to S  (correct, same repo A)
    // File B at "src/lib.rs" WITHOUT a DEFINES edge to S (different repo B)
    //
    // Since File A has a DEFINES edge to S, the primary DEFINES-based lookup
    // is used. File B must NOT appear in source_facts.
    let sym_id = "codegraph:v4:multirepo_sym001";
    let sym = ctx_symbol(sym_id, "multirepo_fn", "src/lib.rs", 1);

    let correct_file_id = aletheia_egregore::ir::stable_id(&["file", "repoa:src/lib.rs"]);
    let correct_file = GraphRecord::node(
        correct_file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs (repo A)".to_owned(),
    );

    let foreign_file_id = aletheia_egregore::ir::stable_id(&["file", "repob:src/lib.rs"]);
    let foreign_file = GraphRecord::node(
        foreign_file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs (repo B)".to_owned(),
    );

    // DEFINES edge: correct_file → Symbol S (repo A only)
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        correct_file_id.clone(),
        sym_id.to_owned(),
        None,
        "file A defines symbol".to_owned(),
    );

    let records = vec![sym, correct_file, foreign_file, defines_edge];
    let ctx = symbol_context(&records, "multirepo_fn");

    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == correct_file_id.as_str()),
        "correct_file must be in source_facts (has DEFINES edge to symbol)"
    );
    assert!(
        ctx.source_facts
            .iter()
            .all(|r| r.id() != foreign_file_id.as_str()),
        "foreign_file must NOT be in source_facts (no DEFINES edge; different repo)"
    );
}

// ── Finding: evidence_link arm must only match seed_ids, not any frontier node ──
//
// The evidence_link arm fires when a node's evidence_links point at the current
// frontier. When a shared Verification run enters the frontier via a VALIDATED_BY
// edge traversal, a sibling observation that cites only that run via evidence_links
// must NOT be pulled in — it has no direct link to the queried symbol/file seeds.
// Restrict the arm to seed_ids only.

#[allow(clippy::too_many_lines)]
#[test]
fn symbol_context_evidence_link_to_shared_run_not_pulled_in_via_evidence_link_arm() {
    // linked_obs --MENTIONS_SYMBOL(edge)--> S   (hop 1: linked_obs via edge arm)
    // linked_obs --VALIDATED_BY(edge)----> Run  (hop 2: Run in frontier)
    // sibling_obs.evidence_links = [{target_record_id: run_id}]
    //
    // With frontier check: sibling_obs fires (run_id in frontier after hop 2).
    // With seed_ids check: sibling_obs does NOT fire (run_id not in seed_ids).
    let sym_id = "codegraph:v4:ev_link_fwd_sym001";
    let sym = ctx_symbol(sym_id, "ev_link_fwd_fn", "src/lib.rs", 1);

    let linked_obs_id = agent_memory_stable_id(&["obs", "ev_link_fwd_obs_linked"]);
    let mut linked_obs = GraphRecord::node(
        linked_obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation about ev_link_fwd_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = linked_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    let run_id = aletheia_egregore::ir::verification_stable_id(&["run", "ev_link_fwd_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run shared between linked and sibling obs".to_owned(),
    );

    // sibling_obs cites only the run via an evidence_link — no link to the symbol.
    let sibling_obs_id = agent_memory_stable_id(&["obs", "ev_link_fwd_obs_sibling"]);
    let mut sibling_obs = GraphRecord::node(
        sibling_obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "sibling observation that cites only the run, not the symbol".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = sibling_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.8".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(run_id.clone()),
            target_domain: "verification".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // linked_obs --MENTIONS_SYMBOL edge--> S
    let sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        linked_obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "linked_obs mentions ev_link_fwd_fn".to_owned(),
    );
    // linked_obs --VALIDATED_BY edge--> Run
    let val_edge = GraphRecord::edge(
        EdgeLabel::ValidatedBy,
        linked_obs_id.clone(),
        run_id.clone(),
        None,
        "linked_obs validated by run".to_owned(),
    );

    let records = vec![sym, linked_obs, sibling_obs, run, sym_edge, val_edge];
    let ctx = symbol_context(&records, "ev_link_fwd_fn");

    assert!(
        ctx.observations
            .iter()
            .any(|r| r.id() == linked_obs_id.as_str()),
        "linked_obs must be in observations (linked to symbol via edge)"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "run must be in verification_evidence"
    );
    assert!(
        ctx.observations
            .iter()
            .all(|r| r.id() != sibling_obs_id.as_str()),
        "sibling_obs must NOT appear — its evidence_link targets only the run, not the symbol"
    );
}

// ── Finding: triple-based evidence links must surface as unresolved ────────────
//
// An EvidenceLink may carry (target_repo_relative_path, target_span,
// target_git_commit) instead of target_record_id when the writer cannot
// compute the stable hash. Such links must be surfaced in `unresolved` so
// consumers can diagnose the absent target, rather than silently discarded.

#[test]
fn symbol_context_triple_evidence_link_surfaced_as_unresolved() {
    // Obs has two evidence_links:
    //   1. target_record_id = sym_id (present seed) — normal resolved link
    //   2. target_repo_relative_path + target_git_commit (triple form, no target_record_id)
    //
    // The triple link must appear in unresolved with a constructed handle.
    let sym_id = agent_memory_stable_id(&["sym", "triple_ev_link_sym"]);
    let sym = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/triple.rs".to_owned()),
        None,
        Some("triple_ev_fn".to_owned()),
        "fn triple_ev_fn".to_owned(),
    );

    let obs_id = agent_memory_stable_id(&["obs", "triple_ev_obs"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "observation with triple evidence link".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            // Resolved link: points at the symbol seed
            EvidenceLink {
                target_record_id: Some(sym_id),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
            // Triple-based link: no target_record_id, uses path+commit form
            EvidenceLink {
                target_record_id: None,
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.7".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: Some("src/other.rs".to_owned()),
                target_span: None,
                target_git_commit: Some("deadbeef".to_owned()),
            },
        ]);
    }

    let records = vec![sym, obs];
    let ctx = symbol_context(&records, "triple_ev_fn");

    assert!(
        !ctx.observations.is_empty(),
        "observation must be in observations"
    );
    assert!(
        !ctx.unresolved.is_empty(),
        "triple-based evidence link must surface as unresolved"
    );
    let has_triple_handle = ctx
        .unresolved
        .iter()
        .any(|u| u.target_handle.contains("src/other.rs") && u.target_handle.contains("deadbeef"));
    assert!(
        has_triple_handle,
        "unresolved entry must carry path+commit handle; got: {:?}",
        ctx.unresolved
    );
}

// ── Finding: path fallback must skip tombstoned file nodes ───────────────────
//
// When no DEFINES edge exists for a symbol, the path-based fallback adds every
// File node whose `repo_relative_path` matches the symbol's path. A tombstoned
// file record must not be seeded because it would pollute seed_ids and could
// cause deleted file context to leak into an otherwise live symbol query.

#[test]
fn symbol_context_path_fallback_excludes_tombstoned_file() {
    // Symbol S at "src/foo.rs" — no DEFINES edge
    // File F1 (live) at "src/foo.rs"  — must be in source_facts
    // File F2 (tombstoned) at "src/foo.rs" — must NOT be in source_facts
    let sym_id = "codegraph:v4:pathfb_tomb_sym001";
    let sym = ctx_symbol(sym_id, "pathfb_tomb_fn", "src/foo.rs", 1);

    let live_file_id = aletheia_egregore::ir::stable_id(&["file", "pathfb_tomb_live"]);
    let live_file = GraphRecord::node(
        live_file_id.clone(),
        NodeKind::File,
        Some("src/foo.rs".to_owned()),
        None,
        None,
        "src/foo.rs live".to_owned(),
    );

    let dead_file_id = aletheia_egregore::ir::stable_id(&["file", "pathfb_tomb_dead"]);
    let dead_file = GraphRecord::node(
        dead_file_id.clone(),
        NodeKind::File,
        Some("src/foo.rs".to_owned()),
        None,
        None,
        "src/foo.rs dead".to_owned(),
    );
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:pathfb_tomb_dead".to_owned(),
        schema_version: 0,
        deleted_id: dead_file_id.clone(),
        summary: "file removed".to_owned(),
        producer: None,
    };

    let records = vec![sym, live_file, dead_file, tombstone];
    let ctx = symbol_context(&records, "pathfb_tomb_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == live_file_id.as_str()),
        "live file must be in source_facts via path fallback"
    );
    assert!(
        ctx.source_facts
            .iter()
            .all(|r| r.id() != dead_file_id.as_str()),
        "tombstoned file must NOT appear in source_facts"
    );
}

// ── Finding: partial DEFINES coverage must fall back per-symbol ───────────────
//
// When multiple live symbols share the queried name and only one of them has a
// DEFINES edge, the global `files_added_via_defines` flag would disable the
// path fallback for all matches. The symbol whose file was not resolved via
// DEFINES would then be missing its co-located file from source_facts. Track
// per-symbol DEFINES coverage instead.

#[test]
fn symbol_context_partial_defines_per_symbol_path_fallback() {
    // Two symbols named "partial_defines_fn": sym_with_defines at "src/a.rs" (has DEFINES),
    // sym_no_defines at "src/b.rs" (no DEFINES edge, but a live file exists at that path).
    // Both files must appear in source_facts.
    let sym_with_defines_id = "codegraph:v4:partial_defines_sym_a";
    let sym_with_defines = ctx_symbol(sym_with_defines_id, "partial_defines_fn", "src/a.rs", 1);

    let sym_no_defines_id = "codegraph:v4:partial_defines_sym_b";
    let sym_no_defines = ctx_symbol(sym_no_defines_id, "partial_defines_fn", "src/b.rs", 10);

    let file_defines_id = aletheia_egregore::ir::stable_id(&["file", "partial_defines_a"]);
    let file_defines = GraphRecord::node(
        file_defines_id.clone(),
        NodeKind::File,
        Some("src/a.rs".to_owned()),
        None,
        None,
        "src/a.rs".to_owned(),
    );

    let file_fallback_id = aletheia_egregore::ir::stable_id(&["file", "partial_defines_b"]);
    let file_fallback = GraphRecord::node(
        file_fallback_id.clone(),
        NodeKind::File,
        Some("src/b.rs".to_owned()),
        None,
        None,
        "src/b.rs".to_owned(),
    );

    // DEFINES edge only for sym_with_defines; sym_no_defines has none
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_defines_id.clone(),
        sym_with_defines_id.to_owned(),
        None,
        "file_defines defines sym_with_defines".to_owned(),
    );

    let records = vec![
        sym_with_defines,
        sym_no_defines,
        file_defines,
        file_fallback,
        defines_edge,
    ];
    let ctx = symbol_context(&records, "partial_defines_fn");

    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == file_defines_id.as_str()),
        "file resolved via DEFINES must be in source_facts"
    );
    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == file_fallback_id.as_str()),
        "file for sym_no_defines must be in source_facts via path fallback"
    );
}

// ── Finding: backfill expansion must loop until convergence ───────────────────
//
// When a node is classified only by the post-BFS evidence-link backfill (e.g.
// Task discovered via Obs evidence_links), the extra edge pass must continue
// traversing from newly discovered nodes until no new nodes are classified.
// A single pass finds Task → AC but misses AC → CommandRun.

#[allow(clippy::too_many_lines)]
#[test]
fn symbol_context_backfill_ac_closes_verification_via_extra_hop() {
    // Symbol S
    // Obs --MENTIONS_SYMBOL edge--> S              (Obs: edge arm, hop 1)
    // Obs.evidence_links = [task_id]               (Task: backfill classification)
    // Task --OWNED_BY_TASK edge--> AC              (AC: backfill edge pass 1)
    // AC --CLOSES_ACCEPTANCE_CRITERION--> Run      (Run: needs backfill edge pass 2)
    let sym_id = "codegraph:v4:backfill_close_sym001";
    let sym = ctx_symbol(sym_id, "backfill_close_fn", "src/lib.rs", 1);

    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "backfill_close_task"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Backfill close task".to_owned()),
        "Task: Backfill close task".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ..
    } = task
    {
        *title = Some("Backfill close task".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
    }

    let ac_id = aletheia_egregore::ir::project_stable_id(&["ac", "backfill_close_ac"]);
    let ac = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("AC for backfill close".to_owned()),
        "AC for backfill close".to_owned(),
    );

    let run_id = aletheia_egregore::ir::verification_stable_id(&["run", "backfill_close_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run closing the AC".to_owned(),
    );

    // Obs: classified via edge arm; has evidence_link to Task
    let obs_id = agent_memory_stable_id(&["obs", "backfill_close_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation about backfill_close_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(task_id.clone()),
            target_domain: "project".to_owned(),
            relation: "REFERENCES_TASK".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Obs --MENTIONS_SYMBOL edge--> S
    let obs_sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id,
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs mentions backfill_close_fn".to_owned(),
    );
    // Task --OWNED_BY_TASK--> AC
    let owned_edge = GraphRecord::edge(
        EdgeLabel::OwnedByTask,
        task_id.clone(),
        ac_id.clone(),
        None,
        "task owns AC".to_owned(),
    );
    // AC --CLOSES_ACCEPTANCE_CRITERION--> Run
    let closes_edge = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_id.clone(),
        run_id.clone(),
        None,
        "AC closed by run".to_owned(),
    );

    let records = vec![
        sym,
        obs,
        task,
        ac,
        run,
        obs_sym_edge,
        owned_edge,
        closes_edge,
    ];
    let ctx = symbol_context(&records, "backfill_close_fn");

    assert!(
        ctx.project_state.iter().any(|r| r.id() == task_id.as_str()),
        "task must be in project_state (backfill from obs evidence_link)"
    );
    assert!(
        ctx.project_state.iter().any(|r| r.id() == ac_id.as_str()),
        "AC must be in project_state (backfill edge pass 1 from task)"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "run must be in verification_evidence (backfill edge pass 2 from AC via CLOSES_ACCEPTANCE_CRITERION)"
    );
}

// ── Finding: EXPLAINS_CHANGE must not be forward-only — backward traversal needed ─
//
// Schema direction: Observation --EXPLAINS_CHANGE--> Symbol/File.
// The symbol/file is the target; querying it must discover the explaining
// Observation via backward traversal (frontier contains target → classify source).
// Making EXPLAINS_CHANGE forward-only blocks this discovery. The fix mirrors
// the MentionsSymbol pattern, which already uses backward traversal correctly.

#[test]
fn symbol_context_explains_change_edge_traverses_backward_from_symbol() {
    // Observation O has an EXPLAINS_CHANGE edge to Symbol S.
    // Querying S must discover O in observations via backward edge traversal.
    let sym_id = "codegraph:v4:explains_change_sym001";
    let sym = ctx_symbol(sym_id, "explains_change_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "explains_change_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation explaining the change to explains_change_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    // Obs --EXPLAINS_CHANGE--> Symbol (Obs is source, Symbol is target)
    let explains_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::ExplainsChange,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs explains change to explains_change_fn".to_owned(),
    );

    let records = vec![sym, obs, explains_edge];
    let ctx = symbol_context(&records, "explains_change_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "observation must appear in observations via backward EXPLAINS_CHANGE traversal from symbol seed"
    );
}

// ── Finding: DEFINES edge with absent file source must not seed source_facts ───
//
// When a DEFINES edge references a file node that is absent from the current
// graph slice (stale edge from a previous scan), adding its source ID to
// source_facts and therefore seed_ids can cause unrelated cross-domain edges
// that happen to reference that ghost ID to pull in unrelated context.
// Verify that the source is a live File node before seeding.

#[test]
fn symbol_context_defines_edge_absent_file_source_not_in_source_facts() {
    // Symbol S at "src/lib.rs"
    // DEFINES edge: ghost_file_id --DEFINES--> S
    // No File node record for ghost_file_id exists in the records slice.
    // No other files in the slice.
    //
    // Without the fix: ghost_file_id ends up in source_facts/seed_ids.
    // With the fix: DEFINES scan skips the absent source; files_added_via_defines
    // stays false; path-based fallback also finds no File node → S is the only
    // record in source_facts.
    let sym_id = "codegraph:v4:absent_file_sym001";
    let sym = ctx_symbol(sym_id, "absent_file_fn", "src/lib.rs", 1);

    let ghost_file_id = "codegraph:v4:ghost_file_absent";

    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        ghost_file_id.to_owned(),
        sym_id.to_owned(),
        None,
        "stale DEFINES edge to absent file".to_owned(),
    );

    let records = vec![sym, defines_edge];
    let ctx = symbol_context(&records, "absent_file_fn");

    assert!(
        !ctx.is_no_match(),
        "symbol must be found even with stale DEFINES edge"
    );
    assert!(
        ctx.source_facts.iter().all(|r| r.id() != ghost_file_id),
        "ghost file ID must NOT appear in source_facts"
    );
    assert_eq!(
        ctx.source_facts.len(),
        1,
        "only the symbol itself should be in source_facts; got: {:?}",
        ctx.source_facts.iter().map(|r| r.id()).collect::<Vec<_>>()
    );
}

// ── Finding: tombstoned DEFINES edge must not seed the file source ─────────────
//
// An incremental scan that tombstones a stale DEFINES edge (e.g. after a symbol
// move) may still have the old File node present. The current code checks the
// FILE's tombstone but not the EDGE's own tombstone, so the old file is wrongly
// seeded. Check the edge record's own ID against tombstoned_ids.

#[test]
fn symbol_context_tombstoned_defines_edge_does_not_seed_stale_file() {
    // Symbol S at "src/new.rs"
    // old_file at "src/old.rs" — DEFINES edge to S is tombstoned (symbol moved)
    // new_file at "src/new.rs" — live DEFINES edge to S
    // Expected: only new_file in source_facts (old DEFINES edge tombstoned)
    let sym_id = "codegraph:v4:tomb_defines_sym001";
    let sym = ctx_symbol(sym_id, "tomb_defines_fn", "src/new.rs", 1);

    let old_file_id = aletheia_egregore::ir::stable_id(&["file", "tomb_defines_old"]);
    let old_file = GraphRecord::node(
        old_file_id.clone(),
        NodeKind::File,
        Some("src/old.rs".to_owned()),
        None,
        None,
        "src/old.rs".to_owned(),
    );

    let new_file_id = aletheia_egregore::ir::stable_id(&["file", "tomb_defines_new"]);
    let new_file = GraphRecord::node(
        new_file_id.clone(),
        NodeKind::File,
        Some("src/new.rs".to_owned()),
        None,
        None,
        "src/new.rs".to_owned(),
    );

    // Old DEFINES edge: old_file → S — this edge is tombstoned
    let old_edge_id = aletheia_egregore::ir::stable_id(&["edge", "DEFINES", &old_file_id, sym_id]);
    let old_defines = GraphRecord::edge(
        EdgeLabel::Defines,
        old_file_id.clone(),
        sym_id.to_owned(),
        None,
        "old file defines symbol (stale)".to_owned(),
    );
    // Tombstone for the old DEFINES edge
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:tomb_defines_edge".to_owned(),
        schema_version: 0,
        deleted_id: old_edge_id,
        summary: "DEFINES edge removed after symbol moved to src/new.rs".to_owned(),
        producer: None,
    };

    // Live DEFINES edge: new_file → S
    let new_defines = GraphRecord::edge(
        EdgeLabel::Defines,
        new_file_id.clone(),
        sym_id.to_owned(),
        None,
        "new file defines symbol".to_owned(),
    );

    let records = vec![
        sym,
        old_file,
        new_file,
        old_defines,
        edge_tombstone,
        new_defines,
    ];
    let ctx = symbol_context(&records, "tomb_defines_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.source_facts
            .iter()
            .any(|r| r.id() == new_file_id.as_str()),
        "new_file must be in source_facts via live DEFINES edge"
    );
    assert!(
        ctx.source_facts
            .iter()
            .all(|r| r.id() != old_file_id.as_str()),
        "old_file must NOT be in source_facts — its DEFINES edge was tombstoned"
    );
}

// ── Finding: ToolCall relay must expand BFS frontier despite no context section ─
//
// ToolCall is not classified into any context section (it is infrastructure), but
// it bridges TOUCHED_FILE → file seeds to PRODUCED_EVIDENCE → verification nodes.
// When ToolCall is reached via backward TOUCHED_FILE traversal from a File seed,
// `was_classified` is false and the current code stops traversal before the
// PRODUCED_EVIDENCE edge, silently omitting the CommandRun/TestRun from
// verification_evidence. Allow relay node kinds to continue the BFS frontier.

#[test]
fn symbol_context_tool_call_relay_discovers_produced_evidence() {
    // Symbol S, File F (DEFINES edge F → S so F is a seed)
    // ToolCall TC: TOUCHED_FILE edge → F (backward from F discovers TC)
    // ToolCall TC: PRODUCED_EVIDENCE edge → CommandRun CR
    // Expected: CR appears in verification_evidence
    let sym_id = "codegraph:v4:toolcall_relay_sym001";
    let sym = ctx_symbol(sym_id, "toolcall_relay_fn", "src/lib.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "toolcall_relay_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs".to_owned(),
    );

    // DEFINES edge: File → Symbol (seeds the file)
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "file defines symbol".to_owned(),
    );

    let tc_id = aletheia_egregore::ir::stable_id(&["toolcall", "toolcall_relay_tc"]);
    let tc = GraphRecord::node(
        tc_id.clone(),
        NodeKind::ToolCall,
        None,
        None,
        None,
        "tool call that touched lib.rs".to_owned(),
    );

    let run_id = aletheia_egregore::ir::verification_stable_id(&["run", "toolcall_relay_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run produced by tool call".to_owned(),
    );

    // ToolCall --TOUCHED_FILE--> File (backward traversal from File seed discovers TC)
    let touched_edge = GraphRecord::edge(
        EdgeLabel::TouchedFile,
        tc_id.clone(),
        file_id.clone(),
        None,
        "tool call touched lib.rs".to_owned(),
    );
    // ToolCall --PRODUCED_EVIDENCE--> CommandRun (should be traversed from TC relay)
    let produced_edge = GraphRecord::edge(
        EdgeLabel::ProducedEvidence,
        tc_id,
        run_id.clone(),
        None,
        "tool call produced command run".to_owned(),
    );

    let records = vec![
        sym,
        file,
        defines_edge,
        tc,
        run,
        touched_edge,
        produced_edge,
    ];
    let ctx = symbol_context(&records, "toolcall_relay_fn");

    assert!(
        ctx.source_facts.iter().any(|r| r.id() == file_id.as_str()),
        "file must be in source_facts"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "CommandRun must be in verification_evidence via ToolCall relay (TOUCHED_FILE → ToolCall → PRODUCED_EVIDENCE → CommandRun)"
    );
}

// ── Finding: temporal DEFINES edge and file source must survive tombstone ─────
//
// When scan-history records carry temporal metadata, the file and DEFINES edge
// may share their stable IDs with current-state records that are tombstoned.
// The tombstone exclusion must not suppress temporal (historical) file seeds or
// their DEFINES edges — only current-state (non-temporal) records should be
// excluded by tombstones.

#[test]
fn symbol_context_temporal_defines_edge_and_file_survive_tombstone() {
    // Symbol S (temporal: historical snapshot at commit A)
    // File F (temporal: same ID, tombstoned in current state)
    // DEFINES edge (temporal: same ID, tombstoned in current state) F → S
    // Tombstones for both file ID and edge ID (current-state deletions)
    // Expected: temporal F appears in source_facts — historical context preserved.
    let sym_id = "codegraph:v4:temporal_defines_sym001";
    let sym = ctx_symbol(sym_id, "temporal_defines_fn", "src/lib.rs", 1)
        .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    let file_id = aletheia_egregore::ir::stable_id(&["file", "temporal_defines_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs (temporal)".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    // DEFINES edge (temporal): file → symbol
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "temporal defines edge".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let defines_edge_id = defines_edge.id().to_owned();

    // Tombstones simulating current-state deletion of both file and edge
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_defines_edge".to_owned(),
        schema_version: 0,
        deleted_id: defines_edge_id,
        summary: "DEFINES edge deleted in current state".to_owned(),
        producer: None,
    };
    let file_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_defines_file".to_owned(),
        schema_version: 0,
        deleted_id: file_id.clone(),
        summary: "file deleted in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym, file, defines_edge, edge_tombstone, file_tombstone];
    let ctx = symbol_context(&records, "temporal_defines_fn");

    assert!(!ctx.is_no_match(), "temporal symbol must be found");
    assert!(
        ctx.source_facts.iter().any(|r| r.id() == file_id.as_str()),
        "temporal file must appear in source_facts despite its ID being tombstoned in current state"
    );
}

// ── Finding: AgentTurn relay must expand BFS frontier ────────────────────────
//
// AgentTurn has no context section (classified as None), but it bridges
// TOUCHES_FILE edges to file seeds and PRODUCED_PATCH edges to Artifact nodes.
// Without AgentTurn in is_bfs_relay_node(), BFS stops at the AgentTurn and the
// PatchArtifact is silently omitted from artifacts.

#[test]
fn symbol_context_agent_turn_relay_discovers_produced_patch() {
    // Symbol S, File F (DEFINES edge F → S so F is a seed)
    // AgentTurn AT: TOUCHES_FILE edge AT → F (backward from F discovers AT)
    // AgentTurn AT: PRODUCED_PATCH edge AT → PatchArtifact P
    // Expected: P appears in artifacts
    let sym_id = "codegraph:v4:agent_turn_relay_sym001";
    let sym = ctx_symbol(sym_id, "agent_turn_relay_fn", "src/lib.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "agent_turn_relay_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs".to_owned(),
    );

    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "file defines symbol".to_owned(),
    );

    let at_id =
        aletheia_egregore::ir::agent_memory_stable_id(&["agentturn", "agent_turn_relay_at"]);
    let at = GraphRecord::node(
        at_id.clone(),
        NodeKind::AgentTurn,
        None,
        None,
        None,
        "agent turn that touched lib.rs".to_owned(),
    );

    let patch_id =
        aletheia_egregore::ir::agent_memory_stable_id(&["patch", "agent_turn_relay_patch"]);
    let patch = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "patch produced by agent turn".to_owned(),
    );

    // AgentTurn --TOUCHES_FILE--> File (backward from File seed discovers AT)
    let touches_edge = GraphRecord::edge(
        EdgeLabel::TouchesFile,
        at_id.clone(),
        file_id.clone(),
        None,
        "agent turn touched lib.rs".to_owned(),
    );
    // AgentTurn --PRODUCED_PATCH--> PatchArtifact (forward from AT relay)
    let produced_edge = GraphRecord::edge(
        EdgeLabel::ProducedPatch,
        at_id,
        patch_id.clone(),
        None,
        "agent turn produced patch".to_owned(),
    );

    let records = vec![
        sym,
        file,
        defines_edge,
        at,
        patch,
        touches_edge,
        produced_edge,
    ];
    let ctx = symbol_context(&records, "agent_turn_relay_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.source_facts.iter().any(|r| r.id() == file_id.as_str()),
        "file must be in source_facts"
    );
    assert!(
        ctx.artifacts.iter().any(|r| r.id() == patch_id.as_str()),
        "PatchArtifact must be in artifacts via AgentTurn relay (TOUCHES_FILE → AgentTurn → PRODUCED_PATCH → PatchArtifact)"
    );
}

// ── Finding: tombstoned cross-domain edge must not be traversed ───────────────
//
// During BFS edge traversal the edge record's own ID is not checked against
// tombstoned_ids. A tombstoned edge (e.g. a stale MENTIONS_SYMBOL edge after an
// observation was retracted) would still carry the BFS to the linked node,
// silently including retracted context. Check the edge ID before traversal.

#[test]
fn symbol_context_tombstoned_cross_domain_edge_not_traversed() {
    // Symbol S
    // Observation O — connected via MENTIONS_SYMBOL edge that is tombstoned
    // Expected: O does NOT appear in observations (edge tombstoned → traversal skipped)
    let sym_id = "codegraph:v4:tomb_edge_sym001";
    let sym = ctx_symbol(sym_id, "tomb_edge_fn", "src/lib.rs", 1);

    let obs_id = aletheia_egregore::ir::agent_memory_stable_id(&["obs", "tomb_edge_obs"]);
    let obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation that mentions tomb_edge_fn".to_owned(),
    );

    // MENTIONS_SYMBOL edge O → S (will be tombstoned)
    let edge = GraphRecord::edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        None,
        "observation mentions symbol".to_owned(),
    );
    let edge_id = edge.id().to_owned();

    // Tombstone for the edge (observation retracted or edge invalidated)
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:tomb_edge_edge".to_owned(),
        schema_version: 0,
        deleted_id: edge_id,
        summary: "MENTIONS_SYMBOL edge tombstoned".to_owned(),
        producer: None,
    };

    let records = vec![sym, obs, edge, edge_tombstone];
    let ctx = symbol_context(&records, "tomb_edge_fn");

    assert!(!ctx.is_no_match(), "symbol must still be found");
    assert!(
        ctx.observations.iter().all(|r| r.id() != obs_id.as_str()),
        "observation must NOT appear in observations — its edge to the symbol was tombstoned"
    );
}

// ── Finding: resolve must exclude tombstoned non-temporal record versions ─────
//
// When a record ID has both a temporal (historical) version and a non-temporal
// (current-state) version that is tombstoned, `resolve` iterates all records
// matching the ID and currently returns both. The tombstoned current-state
// version must be filtered out so only the temporal snapshot appears in output.

#[test]
fn symbol_context_resolve_excludes_tombstoned_current_version() {
    // Symbol S shares an ID with a temporal (historical) and a current-state version.
    // The current-state version is tombstoned.
    // Expected: source_facts contains only the temporal version, not the tombstoned one.
    let sym_id = "codegraph:v4:resolve_tomb_sym001";
    // Temporal (historical) version — survives tombstone
    let sym_temporal = ctx_symbol(sym_id, "resolve_tomb_fn", "src/lib.rs", 1)
        .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    // Current-state (non-temporal) version — tombstoned
    let sym_current = ctx_symbol(sym_id, "resolve_tomb_fn", "src/lib.rs", 1);
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:resolve_tomb_sym".to_owned(),
        schema_version: 0,
        deleted_id: sym_id.to_owned(),
        summary: "symbol deleted in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym_temporal, sym_current, tombstone];
    let ctx = symbol_context(&records, "resolve_tomb_fn");

    assert!(!ctx.is_no_match(), "temporal symbol must be found");
    let temporal_count = ctx.source_facts.iter().filter(|r| r.id() == sym_id).count();
    assert_eq!(
        temporal_count, 1,
        "only the temporal version should appear in source_facts; tombstoned current-state version must be excluded"
    );
    assert!(
        ctx.source_facts.iter().any(|r| {
            r.id() == sym_id
                && matches!(
                    r,
                    GraphRecord::Node {
                        temporal: Some(_),
                        ..
                    }
                )
        }),
        "the surviving source_facts entry for sym_id must be the temporal version"
    );
}

// ── Finding: temporal context node must survive tombstone in classify_and_insert
//
// classify_and_insert gates on tombstoned_ids without checking whether the
// candidate record has temporal metadata. A historical Observation with the same
// stable ID as a current-state tombstone would be silently dropped, losing
// historical context. Mirror the symbol handling: allow temporal records through
// even when their ID appears in tombstoned_ids.

#[test]
fn symbol_context_temporal_context_node_survives_tombstone() {
    // Symbol S
    // Observation O (temporal, commit A): MENTIONS_SYMBOL edge to S
    // Tombstone for O's ID (current-state O was deleted)
    // Expected: temporal O appears in observations
    let sym_id = "codegraph:v4:temporal_ctx_node_sym001";
    let sym = ctx_symbol(sym_id, "temporal_ctx_node_fn", "src/lib.rs", 1);

    let obs_id = aletheia_egregore::ir::agent_memory_stable_id(&["obs", "temporal_ctx_node_obs"]);
    let obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "historical observation about temporal_ctx_node_fn".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    // MENTIONS_SYMBOL edge: O → S (live edge, symbol seed triggers backward traversal to O)
    let edge = GraphRecord::edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        None,
        "historical observation mentions symbol".to_owned(),
    );

    // Tombstone for O's ID (current-state deletion)
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_ctx_node_obs".to_owned(),
        schema_version: 0,
        deleted_id: obs_id.clone(),
        summary: "observation deleted in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym, obs, edge, tombstone];
    let ctx = symbol_context(&records, "temporal_ctx_node_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "temporal observation must appear in observations despite its ID being tombstoned in current state"
    );
}

// ── Finding: triple-only evidence links must be surfaced as unresolved ────────
//
// When an agent-memory node cites the queried symbol/file ONLY via the unresolved
// triple form (target_repo_relative_path/target_span/target_git_commit with no
// resolved target_record_id), the `links_to_frontier` gate is false and the
// entire node is skipped, so the triple is never added to `unresolved`. Nodes
// that only have triple-form citations must still have those triples surfaced.

#[test]
fn symbol_context_triple_only_evidence_link_surfaced_as_unresolved() {
    // Symbol S at "src/lib.rs"
    // Observation O: evidence_link with triple form ONLY (no target_record_id),
    //   target_repo_relative_path: "src/lib.rs"
    // Expected: the triple appears in ctx.unresolved; O is never classified since
    //   we cannot confirm the triple resolves to this exact symbol record.
    let sym_id = "codegraph:v4:triple_only_sym001";
    let sym = ctx_symbol(sym_id, "triple_only_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "triple_only_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation citing symbol via triple form only".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.8".to_owned());
        // Triple-form only: no target_record_id, only path (and optionally span/commit)
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: None,
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.8".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/lib.rs".to_owned()),
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![sym, obs];
    let ctx = symbol_context(&records, "triple_only_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.unresolved
            .iter()
            .any(|u| u.source_record_id == obs_id && u.target_handle == "src/lib.rs"),
        "triple-form citation must appear in unresolved even when target_record_id is absent; \
         got: {:?}",
        ctx.unresolved
    );
}

// ── Finding: temporal relay node must survive tombstone ───────────────────────
//
// is_bfs_relay_node() applies an unconditional tombstone guard that rejects relay
// nodes (ToolCall/AgentTurn/AgentRun) whose stable ID is tombstoned in the current
// state. Temporal (historical) relay nodes with the same stable ID must be exempt
// from this suppression, matching the temporal guard added to classify_and_insert.

#[allow(clippy::too_many_lines)]
#[test]
fn symbol_context_temporal_relay_node_survives_tombstone() {
    // Symbol S, File F (DEFINES edge F → S so F is a seed)
    // ToolCall TC (temporal, commit A): TOUCHES_FILE edge → F (backward from F → TC)
    // TC's stable ID is tombstoned in current state
    // TC: PRODUCED_EVIDENCE → CommandRun CR
    // Expected: CR appears in verification_evidence (temporal TC relays despite tombstone)
    let sym_id = "codegraph:v4:temporal_relay_sym001";
    let sym = ctx_symbol(sym_id, "temporal_relay_fn", "src/lib.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "temporal_relay_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs".to_owned(),
    );

    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "file defines symbol".to_owned(),
    );

    let tc_id = aletheia_egregore::ir::agent_memory_stable_id(&["toolcall", "temporal_relay_tc"]);
    let tc = GraphRecord::node(
        tc_id.clone(),
        NodeKind::ToolCall,
        None,
        None,
        None,
        "temporal tool call touching lib.rs".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    let tc_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_relay_tc".to_owned(),
        schema_version: 0,
        deleted_id: tc_id.clone(),
        summary: "ToolCall deleted in current state".to_owned(),
        producer: None,
    };

    let run_id = aletheia_egregore::ir::verification_stable_id(&["run", "temporal_relay_run"]);
    let run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run via temporal tool call".to_owned(),
    );

    // ToolCall (temporal) --TOUCHED_FILE--> File (backward from File seed discovers TC)
    let touched_edge = GraphRecord::edge(
        EdgeLabel::TouchedFile,
        tc_id.clone(),
        file_id,
        None,
        "temporal tool call touched lib.rs".to_owned(),
    );
    // ToolCall (temporal) --PRODUCED_EVIDENCE--> CommandRun (forward from TC relay)
    let produced_edge = GraphRecord::edge(
        EdgeLabel::ProducedEvidence,
        tc_id,
        run_id.clone(),
        None,
        "temporal tool call produced command run".to_owned(),
    );

    let records = vec![
        sym,
        file,
        defines_edge,
        tc,
        tc_tombstone,
        run,
        touched_edge,
        produced_edge,
    ];
    let ctx = symbol_context(&records, "temporal_relay_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "CommandRun must be in verification_evidence via temporal ToolCall relay \
         (temporal ToolCall must not be suppressed by current-state tombstone)"
    );
}

// ── Finding: temporal cross-domain edge must survive tombstone ───────────────
//
// The BFS edge arm's unconditional tombstone check suppresses historical
// cross-domain edges whose stable ID is tombstoned in the current state.
// A temporal edge must be exempt: its tombstone reflects only a current-state
// deletion, not the historical link that is cited in scan-history output.

#[test]
fn symbol_context_temporal_cross_domain_edge_survives_tombstone() {
    // Symbol S (current-state)
    // Observation O (temporal, commit A)
    // MENTIONS_SYMBOL edge E (temporal, commit A): O → S
    // Tombstone for edge E's stable ID (current-state deletion of the edge)
    // Expected: O still appears in observations
    let sym_id = "codegraph:v4:temporal_cde_sym001";
    let sym = ctx_symbol(sym_id, "temporal_cde_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "temporal_cde_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "historical observation about temporal_cde_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
    }

    // Temporal MENTIONS_SYMBOL edge: O → S (historical link, commit A)
    let edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "temporal edge: obs mentions symbol at commit A".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let edge_id = edge.id().to_owned();

    // Current-state tombstone for the edge's stable ID
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_cde_edge".to_owned(),
        schema_version: 0,
        deleted_id: edge_id,
        summary: "edge deleted in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym, obs, edge, edge_tombstone];
    let ctx = symbol_context(&records, "temporal_cde_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "historical observation must appear in observations via temporal edge \
         even though the edge stable ID is tombstoned in current state"
    );
}

// ── Finding: tombstoned non-temporal topology edge must not appear in output ──
//
// When `topology_edge_ids` is populated by a temporal edge (exempt from the
// tombstone guard), the final collection must still exclude any non-temporal
// record with the same stable ID that is in tombstoned_ids. Without filtering,
// a deleted current-state DEFINES edge would be emitted alongside the historical
// one, producing indistinguishable entries for consumers.

#[test]
fn symbol_context_tombstoned_current_topology_edge_excluded_from_output() {
    // Symbol S, File F — both non-temporal (live)
    // DEFINES edge (temporal, commit A): F → S (historical snapshot)
    // DEFINES edge (non-temporal, current): same stable ID (same source/target/label)
    // Tombstone for the DEFINES edge stable ID
    // Expected: topology_edges contains ONLY the temporal version, NOT the non-temporal one
    let sym_id = "codegraph:v4:topo_filter_sym001";
    let sym = ctx_symbol(sym_id, "topo_filter_fn", "src/lib.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "topo_filter_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs".to_owned(),
    );

    // Temporal DEFINES edge: same stable ID will be shared with non-temporal below
    let defines_temporal = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "temporal defines edge".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let defines_edge_id = defines_temporal.id().to_owned();

    // Non-temporal DEFINES edge: same source/target/label → same stable ID
    let defines_current = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        sym_id.to_owned(),
        None,
        "temporal defines edge".to_owned(), // same summary = same ID
    );

    // Tombstone for the shared stable ID (current-state deletion)
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:topo_filter_edge".to_owned(),
        schema_version: 0,
        deleted_id: defines_edge_id.clone(),
        summary: "DEFINES edge tombstoned".to_owned(),
        producer: None,
    };

    let records = vec![sym, file, defines_temporal, defines_current, edge_tombstone];
    let ctx = symbol_context(&records, "topo_filter_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    let topo_count = ctx
        .topology_edges
        .iter()
        .filter(|r| r.id() == defines_edge_id.as_str())
        .count();
    assert_eq!(
        topo_count, 1,
        "topology_edges must contain only the temporal DEFINES edge (not the tombstoned \
         non-temporal version); got {topo_count} records with that stable ID"
    );
    // The one record must be the temporal version
    let is_temporal = ctx.topology_edges.iter().any(|r| {
        r.id() == defines_edge_id.as_str()
            && matches!(
                r,
                GraphRecord::Edge {
                    temporal: Some(_),
                    ..
                }
            )
    });
    assert!(
        is_temporal,
        "the single topology edge in the output must be the temporal version"
    );
}

// ── Finding: triple evidence links must be gated to seed file paths ───────────
//
// The pure-triple else branch (nodes with only triple-form evidence links and no
// resolved target_record_id) adds every triple to `unresolved` without checking
// whether the triple's target_repo_relative_path actually matches a seed file.
// An unrelated agent-memory node citing a DIFFERENT file via triple must not
// pollute the context for the queried symbol.

#[test]
fn symbol_context_unrelated_triple_evidence_link_not_in_unresolved() {
    // Symbol S at "src/lib.rs"
    // Unrelated Observation O: triple evidence link → "src/other.rs" (different file)
    // Expected: the triple does NOT appear in ctx.unresolved
    let sym_id = "codegraph:v4:triple_gate_sym001";
    let sym = ctx_symbol(sym_id, "triple_gate_fn", "src/lib.rs", 1);

    let unrelated_obs_id = agent_memory_stable_id(&["obs", "triple_gate_unrelated"]);
    let mut unrelated_obs = GraphRecord::node(
        unrelated_obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation about other.rs, not lib.rs".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = unrelated_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.8".to_owned());
        // Triple-form only, targeting a DIFFERENT file than the queried symbol
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: None,
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.8".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/other.rs".to_owned()),
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![sym, unrelated_obs];
    let ctx = symbol_context(&records, "triple_gate_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.unresolved
            .iter()
            .all(|u| u.source_record_id != unrelated_obs_id),
        "triple citation targeting 'src/other.rs' must NOT appear in unresolved \
         when querying a symbol in 'src/lib.rs'; got: {:?}",
        ctx.unresolved
    );
}

// ── Finding: temporal topology edge must survive tombstone ────────────────────
//
// The topology_edge_ids collection uses an unconditional tombstone guard. When a
// temporal DEFINES edge and a current-state tombstone share the same stable ID,
// the historical DEFINES edge is omitted from topology_edges even though the
// temporal file and symbol are both in seed_ids (from the now-fixed DEFINES
// seeding loop). Historical context returns the file and symbol but omits the
// citable edge for that commit.

#[test]
fn symbol_context_temporal_topology_edge_survives_tombstone() {
    // Symbol S (temporal, commit A) — same stable ID as current-state
    // File F (temporal, commit A) — same stable ID as current-state (tombstoned)
    // DEFINES edge (temporal, commit A): F → S — same stable ID as current-state (tombstoned)
    // Tombstones for both file and DEFINES edge (current-state deletions)
    // Expected: topology_edges contains the temporal DEFINES edge
    let sym_id = "codegraph:v4:temporal_topo_edge_sym001";
    let sym = ctx_symbol(sym_id, "temporal_topo_edge_fn", "src/lib.rs", 1)
        .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    let file_id = aletheia_egregore::ir::stable_id(&["file", "temporal_topo_edge_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs (temporal)".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));

    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "temporal defines edge".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let defines_edge_id = defines_edge.id().to_owned();

    let file_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_topo_edge_file".to_owned(),
        schema_version: 0,
        deleted_id: file_id,
        summary: "file deleted in current state".to_owned(),
        producer: None,
    };
    let edge_tombstone = GraphRecord::Tombstone {
        id: "tombstone:temporal_topo_edge_edge".to_owned(),
        schema_version: 0,
        deleted_id: defines_edge_id.clone(),
        summary: "DEFINES edge deleted in current state".to_owned(),
        producer: None,
    };

    let records = vec![sym, file, defines_edge, file_tombstone, edge_tombstone];
    let ctx = symbol_context(&records, "temporal_topo_edge_fn");

    assert!(!ctx.is_no_match(), "temporal symbol must be found");
    assert!(
        ctx.topology_edges
            .iter()
            .any(|r| r.id() == defines_edge_id.as_str()),
        "temporal DEFINES topology edge must appear in topology_edges despite its ID being \
         tombstoned in current state"
    );
}

// ── Finding: triple-only else branch must not consume node before edge arm ────
//
// When a node has only triple-form evidence links (no resolved target_record_id),
// the else branch enters and marks the node visited without classifying it. If a
// graph edge connecting that node to the symbol comes later in the records slice
// (the common nodes-before-edges layout), the edge arm finds the node already
// visited and skips classification. The node must NOT be marked visited before
// it is actually classified.

#[test]
fn symbol_context_triple_only_node_not_consumed_before_edge_traversal() {
    // Symbol S at "src/lib.rs"
    // Obs O: ONLY a triple evidence link targeting "src/lib.rs" (no resolved target_record_id)
    //        AND a graph edge O --MENTIONS_SYMBOL--> S
    // Records order: [S, O (node), E (edge)]  — nodes before edges
    // Expected: O in observations via edge arm (triple else branch must not mark O visited)
    let sym_id = "codegraph:v4:triple_consume_sym001";
    let sym = ctx_symbol(sym_id, "triple_consume_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "triple_consume_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "obs with triple link and edge to symbol".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        // Triple-form only: no target_record_id, path matches the symbol's file
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: None,
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/lib.rs".to_owned()),
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Graph edge comes AFTER the node record (nodes-before-edges layout)
    let edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs mentions symbol via edge".to_owned(),
    );

    let records = vec![sym, obs, edge]; // <-- node before edge
    let ctx = symbol_context(&records, "triple_consume_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_id.as_str()),
        "obs must be classified into observations via the graph edge even though it \
         also has a triple-form evidence link; the triple-only else branch must not \
         mark the node visited before the edge arm runs"
    );
}

// ── Finding: relay node reached via evidence_links must expand BFS frontier ───
//
// When a relay node (ToolCall/AgentTurn/AgentRun) is reached through an inline
// evidence link to the seed symbol/file, `classify_and_insert` returns false and
// the evidence_links arm must still add the relay to the frontier so its outgoing
// edges (PRODUCED_EVIDENCE, PRODUCED_PATCH) are traversed in the next hop.

#[test]
fn symbol_context_relay_node_via_evidence_link_expands_frontier() {
    // Symbol S and File F (DEFINES F → S, so F is a seed)
    // ToolCall TC: evidence_link TOUCHED_FILE → F (matches seed F → links_to_frontier=true)
    // TC --PRODUCED_EVIDENCE--> CommandRun CR
    // Expected: CR in verification_evidence (TC must enter frontier despite not being classified)
    let sym_id = "codegraph:v4:relay_evi_sym001";
    let sym = ctx_symbol(sym_id, "relay_evi_fn", "src/lib.rs", 1);

    let file_id = aletheia_egregore::ir::stable_id(&["file", "relay_evi_file"]);
    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "src/lib.rs".to_owned(),
    );

    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id.clone(),
        sym_id.to_owned(),
        None,
        "file defines relay_evi_fn".to_owned(),
    );

    let tc_id = agent_memory_stable_id(&["toolcall", "relay_evi_tc"]);
    let mut tc = GraphRecord::node(
        tc_id.clone(),
        NodeKind::ToolCall,
        None,
        None,
        None,
        "tool call touching relay_evi file".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = tc
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        // Evidence link to seed File F — this puts TC in the evidence_links arm
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(file_id),
            target_domain: "codegraph".to_owned(),
            relation: "TOUCHED_FILE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let run_id = verification_stable_id(&["run", "relay_evi_run"]);
    let mut run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run produced by relay_evi tool call".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }

    // TC --PRODUCED_EVIDENCE--> CR (must be traversed from TC's frontier in next hop)
    let produced_edge = GraphRecord::edge(
        EdgeLabel::ProducedEvidence,
        tc_id,
        run_id.clone(),
        None,
        "tool call produced command run".to_owned(),
    );

    let records = vec![sym, file, defines_edge, tc, run, produced_edge];
    let ctx = symbol_context(&records, "relay_evi_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "CommandRun must be in verification_evidence via relay ToolCall reached through \
         evidence_link — the evidence_links arm must expand relay nodes into the frontier"
    );
}

// ── Finding: each temporal version of a node must have its evidence_links scanned
//
// When multiple temporal records share the same stable ID but have different
// evidence_links, the first version processed marks the stable ID visited and
// all later versions are skipped. For scan-history slices where each commit may
// link different backing evidence, only the first version's evidence is surfaced.

#[test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
fn symbol_context_temporal_observation_evidence_links_scanned_per_version() {
    // Symbol S (current-state)
    // Obs O@commitA (temporal): MENTIONS_SYMBOL → S AND VALIDATED_BY → RunA
    // Obs O@commitB (temporal): MENTIONS_SYMBOL → S AND VALIDATED_BY → RunB
    // RunA, RunB: present CommandRun nodes
    // Expected: both RunA and RunB in verification_evidence
    let sym_id = "codegraph:v4:temporal_evi_scan_sym001";
    let sym = ctx_symbol(sym_id, "temporal_evi_scan_fn", "src/lib.rs", 1);

    let obs_id = agent_memory_stable_id(&["obs", "temporal_evi_scan_obs"]);
    let run_a_id = verification_stable_id(&["run", "temporal_evi_scan_run_a"]);
    let run_b_id = verification_stable_id(&["run", "temporal_evi_scan_run_b"]);

    let make_obs_version = |commit: &str, run_id: &str| -> GraphRecord {
        let mut obs = GraphRecord::node(
            obs_id.clone(),
            NodeKind::Observation,
            None,
            None,
            None,
            format!("temporal observation at commit {commit}"),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            ref mut agent_id,
            ref mut session_id,
            ref mut observed_at,
            ref mut confidence,
            ref mut evidence_links,
            ..
        } = obs
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *agent_id = Some("agent:test".to_owned());
            *session_id = Some("session:test".to_owned());
            *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
            *confidence = Some("0.9".to_owned());
            *evidence_links = Some(vec![
                EvidenceLink {
                    target_record_id: Some(sym_id.to_owned()),
                    target_domain: "codegraph".to_owned(),
                    relation: "MENTIONS_SYMBOL".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
                EvidenceLink {
                    target_record_id: Some(run_id.to_owned()),
                    target_domain: "verification".to_owned(),
                    relation: "VALIDATED_BY".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                },
            ]);
        }
        obs.with_temporal(temporal(commit, "2026-01-01T00:00:00Z"))
    };

    let obs_v1 = make_obs_version("aaaa1111", &run_a_id);
    let obs_v2 = make_obs_version("bbbb2222", &run_b_id);

    let mut run_a = GraphRecord::node(
        run_a_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "run for commit A".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = run_a
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }

    let mut run_b = GraphRecord::node(
        run_b_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "run for commit B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = run_b
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }

    // obs_v1 before obs_v2 in slice — v1 processed first, marks visited,
    // v2 would be skipped without the per-version scan fix
    let records = vec![sym, obs_v1, obs_v2, run_a, run_b];
    let ctx = symbol_context(&records, "temporal_evi_scan_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_a_id.as_str()),
        "RunA must be in verification_evidence (from O@commitA evidence_links)"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_b_id.as_str()),
        "RunB must be in verification_evidence (from O@commitB evidence_links — \
         the second temporal version must be scanned independently)"
    );
}

// ── Finding: backfill must scan evidence_links of newly discovered nodes ──────
//
// The post-BFS backfill snapshots classified nodes and scans their evidence_links
// once. A newly classified target from that scan (e.g. ObsB found via ObsA's
// RELATES_TO evidence_link) has its own evidence_links (e.g. VALIDATED_BY →
// CommandRun) that need scanning, but are skipped because classified_for_backfill
// was captured before ObsB was added. The backfill must be iterative.

#[test]
#[allow(clippy::similar_names, clippy::too_many_lines)]
fn symbol_context_backfill_evidence_links_scanned_recursively() {
    // Symbol S
    // ObsA --MENTIONS_SYMBOL(edge)--> S  (ObsA classified via edge arm)
    // ObsA.evidence_links = [RELATES_TO → ObsB]  (ObsB classified in first backfill pass)
    // ObsB.evidence_links = [VALIDATED_BY → CommandRun CR]  (CR must appear in second pass)
    // Expected: CR in verification_evidence
    let sym_id = "codegraph:v4:backfill_recursive_sym001";
    let sym = ctx_symbol(sym_id, "backfill_recursive_fn", "src/lib.rs", 1);

    let obs_a_id = agent_memory_stable_id(&["obs", "backfill_recursive_obs_a"]);
    let obs_b_id = agent_memory_stable_id(&["obs", "backfill_recursive_obs_b"]);
    let run_id = verification_stable_id(&["run", "backfill_recursive_run"]);

    let mut obs_b = GraphRecord::node(
        obs_b_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "nested observation B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs_b
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.8".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(run_id.clone()),
            target_domain: "verification".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut obs_a = GraphRecord::node(
        obs_a_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation A linking to observation B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs_a
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-01-15T10:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(obs_b_id.clone()),
            target_domain: "agent_memory".to_owned(),
            relation: "RELATES_TO".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run validating observation B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }

    // Edge: ObsA --MENTIONS_SYMBOL--> S (classifies ObsA via edge arm)
    let obs_a_sym_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::MentionsSymbol,
        obs_a_id.clone(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "obs_a mentions backfill_recursive_fn".to_owned(),
    );

    let records = vec![sym, obs_a, obs_b, run, obs_a_sym_edge];
    let ctx = symbol_context(&records, "backfill_recursive_fn");

    assert!(!ctx.is_no_match(), "symbol must be found");
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_a_id.as_str()),
        "ObsA must be in observations (edge arm)"
    );
    assert!(
        ctx.observations.iter().any(|r| r.id() == obs_b_id.as_str()),
        "ObsB must be in observations (first backfill pass via ObsA's evidence_links)"
    );
    assert!(
        ctx.verification_evidence
            .iter()
            .any(|r| r.id() == run_id.as_str()),
        "CommandRun must be in verification_evidence via iterative backfill: \
         ObsA → ObsB → CommandRun (second backfill pass on newly classified ObsB)"
    );
}
