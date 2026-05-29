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
    assert!(ver_in_evidence, "the specific verification record must be in verification_evidence");
    assert!(ctx.unresolved.is_empty(), "no unresolved — all targets are present");
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
    let obs = ctx_observation("tomb_ctx_obs1", "should be hidden", sym_id, "MENTIONS_SYMBOL", "1.0");
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
    let sym = ctx_symbol(sym_id, "hist_fn", "src/hist.rs", 1)
        .with_temporal(TemporalMetadata {
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
    let closes_edge = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ver_id.clone(),
        ac_id.clone(),
        None,
        "verification closes AC".to_owned(),
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

    let records = vec![sym, linked_obs, sibling_obs, run, sym_edge, linked_run_edge, sibling_run_edge];
    let ctx = symbol_context(&records, "sharedrun_fn");

    assert!(
        ctx.observations.iter().any(|r| r.id() == linked_obs_id.as_str()),
        "linked_obs must be in observations (linked to symbol)"
    );
    assert!(
        ctx.verification_evidence.iter().any(|r| r.id() == run_id.as_str()),
        "run must be in verification_evidence (backs linked_obs)"
    );
    assert!(
        ctx.observations.iter().all(|r| r.id() != sibling_obs_id.as_str()),
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
        ctx.verification_evidence.iter().all(|r| r.id() != ver_id.as_str()),
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
        ctx.topology_edges.iter().any(|r| r.id() == defines_edge_id.as_str()),
        "the specific DEFINES edge must be in topology_edges"
    );
}
