# Codex Review Batch 7 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Address five P2 review comments from the Codex bot covering timestamp subsecond precision, naming edit redactions, user-context edge synthesis, graph-to-data-dir writes, and audit trail evidence validation.

**Architecture:**
1. Update `latest_decision_for_candidate` and `decide_candidate` to use millisecond-resolution timestamps.
2. In `decide_candidate`, redact the edited name before materializing naming decisions.
3. Add a helper `synthesize_user_context_edges` in `src/decide.rs` and call it in `src/cli.rs` when executing `decide --data-dir`.
4. In `decide_cmd`, adjust the record loading source if both `--graph` and `--data-dir` are provided.
5. In `audit_trail`, reject supporting evidence that doesn't target the `"agent_memory"` domain with a `"PROPOSED_BY"` relation, or has an invalid node kind.

**Tech Stack:** Rust (edition 2024), Chrono, Blake3, AletheiaDB.

---

### Task 1: Preserve unique prompt IDs within the same second

**Files:**
- Modify: [src/decide.rs](file:///c:/Users/markm/egregore/src/decide.rs)

**Step 1: Write the changes**
Replace:
```rust
    let valid_time_str = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
```
with:
```rust
    let valid_time_str = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
```

**Step 2: Verify it builds**
Run: `$env:CARGO_INCREMENTAL = "0"; $env:CARGO_PROFILE_DEV_DEBUG = "0"; $env:CARGO_PROFILE_TEST_DEBUG = "0"; cargo check`

---

### Task 2: Redact edited naming approvals before storing them

**Files:**
- Modify: [src/decide.rs](file:///c:/Users/markm/egregore/src/decide.rs)

**Step 1: Write the changes**
In `match materialized_kind` for `NodeKind::NamingDecision` arm, replace:
```rust
                    NodeKind::NamingDecision => {
                        durable_fields.entity_kind = cand_fields.entity_kind.clone();
                        durable_fields.canonical_name = if req.outcome == "edited_then_approved" {
                            Some(rule_text.clone())
                        } else {
                            cand_fields.canonical_name.clone()
                        };
                        durable_fields.alternatives_rejected = Some(
                            cand_fields
                                .alternatives_rejected
                                .clone()
                                .unwrap_or_default(),
                        );
                    }
```
with:
```rust
                    NodeKind::NamingDecision => {
                        durable_fields.entity_kind = cand_fields.entity_kind.clone();
                        let canonical = if req.outcome == "edited_then_approved" {
                            redacted_rule_text
                        } else {
                            cand_fields.canonical_name.clone().unwrap_or_default()
                        };
                        if crate::redaction::is_redacted(&canonical) {
                            rule_has_redaction = true;
                        }
                        durable_fields.canonical_name = Some(canonical);
                        durable_fields.alternatives_rejected = Some(
                            cand_fields
                                .alternatives_rejected
                                .clone()
                                .unwrap_or_default(),
                        );
                    }
```

**Step 2: Write failing test**
Add `test_decide_stamps_naming_edited_with_redaction` to [tests/preference_approval.rs](file:///c:/Users/markm/egregore/tests/preference_approval.rs):
```rust
#[test]
fn test_decide_stamps_naming_edited_with_redaction() {
    use aletheia_egregore::decide::{DecideRequest, decide_candidate};
    let cand_id = "naming_candidate";
    let mut candidate = GraphRecord::node(
        cand_id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        "Candidate naming".to_owned(),
    );
    if let GraphRecord::Node { ref mut schema_version, ref mut domain, ref mut user_context, .. } = candidate {
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
    let records = vec![candidate];
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
        if let GraphRecord::Node { kind, user_context, redaction_policy_version, .. } = rec {
            if kind == NodeKind::NamingDecision {
                assert_eq!(redaction_policy_version, Some("v1".to_owned()));
                let name = user_context.canonical_name.as_deref().unwrap();
                assert!(crate::redaction::is_redacted(name));
                naming_checked = true;
            }
        }
    }
    assert!(naming_checked);
}
```

**Step 3: Verify tests pass**
Run: `$env:CARGO_INCREMENTAL = "0"; $env:CARGO_PROFILE_DEV_DEBUG = "0"; $env:CARGO_PROFILE_TEST_DEBUG = "0"; cargo test --test preference_approval test_decide_stamps_naming_edited_with_redaction`

---

### Task 3: Synthesize user-context edges for direct data-dir writes

**Files:**
- Modify: [src/decide.rs](file:///c:/Users/markm/egregore/src/decide.rs)
- Modify: [src/cli.rs](file:///c:/Users/markm/egregore/src/cli.rs)

**Step 1: Write `synthesize_user_context_edges` in [src/decide.rs](file:///c:/Users/markm/egregore/src/decide.rs)**
Add the function:
```rust
pub fn synthesize_user_context_edges(
    records: &[GraphRecord],
    generated: &[GraphRecord],
) -> Vec<GraphRecord> {
    let mut edges = Vec::new();
    let mut prompt_id = None;
    let mut decision_id = None;
    let mut candidate_id = None;
    let mut outcome = None;
    let mut materialized_id = None;

    for rec in generated {
        if let GraphRecord::Node { id, kind, user_context, .. } = rec {
            match kind {
                NodeKind::PromotionPrompt => {
                    prompt_id = Some(id.clone());
                    candidate_id = user_context.candidate_id.clone();
                }
                NodeKind::PromotionDecision => {
                    decision_id = Some(id.clone());
                    outcome = user_context.outcome.clone();
                    materialized_id = user_context.materialized_record_id.clone();
                }
                _ => {}
            }
        }
    }

    if let (Some(p_id), Some(c_id)) = (prompt_id, &candidate_id) {
        let edge_id = user_context_stable_id(&["edge", "PROMPTED_FOR", &p_id, c_id]);
        edges.push(GraphRecord::Edge {
            id: edge_id,
            schema_version: USER_CONTEXT_SCHEMA_VERSION,
            label: crate::ir::EdgeLabel::PromptedFor,
            source: p_id,
            target: c_id.clone(),
            confidence: None,
            temporal: None,
            summary: "PromotionPrompt prompted for PromoteCandidate".to_owned(),
            producer: None,
        });
    }

    if let (Some(d_id), Some(c_id)) = (decision_id.clone(), &candidate_id) {
        let edge_id = user_context_stable_id(&["edge", "DECIDED_ON", &d_id, c_id]);
        edges.push(GraphRecord::Edge {
            id: edge_id,
            schema_version: USER_CONTEXT_SCHEMA_VERSION,
            label: crate::ir::EdgeLabel::DecidedOn,
            source: d_id.clone(),
            target: c_id.clone(),
            confidence: None,
            temporal: None,
            summary: "PromotionDecision decided on PromoteCandidate".to_owned(),
            producer: None,
        });

        if let (Some(out), Some(mat_id)) = (outcome, materialized_id) {
            if out == "approved" || out == "edited_then_approved" {
                let is_revocation = records.iter().any(|r| {
                    if r.id() == c_id {
                        if let GraphRecord::Node { user_context, .. } = r {
                            return user_context.proposed_rule_kind.as_deref() == Some("revocation");
                        }
                    }
                    false
                });

                if is_revocation {
                    let edge_id = user_context_stable_id(&["edge", "REVOKED_BY", &mat_id, &d_id]);
                    edges.push(GraphRecord::Edge {
                        id: edge_id,
                        schema_version: USER_CONTEXT_SCHEMA_VERSION,
                        label: crate::ir::EdgeLabel::RevokedBy,
                        source: mat_id,
                        target: d_id,
                        confidence: None,
                        temporal: None,
                        summary: "PromotionDecision revoked durable user-context record".to_owned(),
                        producer: None,
                    });
                } else {
                    let edge_id = user_context_stable_id(&["edge", "MATERIALIZED_AS", &d_id, &mat_id]);
                    edges.push(GraphRecord::Edge {
                        id: edge_id,
                        schema_version: USER_CONTEXT_SCHEMA_VERSION,
                        label: crate::ir::EdgeLabel::MaterializedAs,
                        source: d_id,
                        target: mat_id,
                        confidence: None,
                        temporal: None,
                        summary: "PromotionDecision materialized durable user-context record".to_owned(),
                        producer: None,
                    });
                }
            }
        }
    }

    edges
}
```

**Step 2: Modify [src/cli.rs](file:///c:/Users/markm/egregore/src/cli.rs) to synthesize and write edges**
Under the `data_dir` block (around line 3603-3608), replace:
```rust
            let mut sink = EmbeddedAletheiaSink::open_unleased(&dir)
                .with_context(|| format!("failed to open embedded store {}", dir.display()))?;
            let report = ingest_records(&generated, &mut sink);
```
with:
```rust
            let mut sink = EmbeddedAletheiaSink::open_unleased(&dir)
                .with_context(|| format!("failed to open embedded store {}", dir.display()))?;
            let edges = crate::decide::synthesize_user_context_edges(&records, &generated);
            let mut all_records = generated.clone();
            all_records.extend(edges);
            let report = ingest_records(&all_records, &mut sink);
```

**Step 3: Write test**
Add `test_decide_synthesizes_edges_for_embedded_write` to `tests/preference_approval.rs`.

**Step 4: Verify test passes**
Run: `$env:CARGO_INCREMENTAL = "0"; $env:CARGO_PROFILE_DEV_DEBUG = "0"; $env:CARGO_PROFILE_TEST_DEBUG = "0"; cargo test`

---

### Task 4: Allow graph-sourced decisions to be written to data-dir

**Files:**
- Modify: [src/cli.rs](file:///c:/Users/markm/egregore/src/cli.rs)

**Step 1: Write the changes**
In `decide_cmd`, replace:
```rust
    let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
```
with:
```rust
    let query_source_dir = if graph.is_some() { None } else { data_dir.as_deref() };
    let records = load_query_records(graph.as_deref(), query_source_dir)?;
```

**Step 2: Verify cargo checks**
Run: `$env:CARGO_INCREMENTAL = "0"; cargo check`

---

### Task 5: Reject non-agent evidence in audit trails

**Files:**
- Modify: [src/query.rs](file:///c:/Users/markm/egregore/src/query.rs)

**Step 1: Write the changes**
In `audit_trail`, replace the loop parsing supporting evidence:
```rust
    let mut obs_nodes = Vec::new();
    for link in supporting {
        let obs_id = link.target_record_id.as_deref().ok_or_else(|| {
            format!(
                "Candidate '{}' supporting evidence link lacks target_record_id",
                candidate_id
            )
        })?;
        let obs = records.iter().find(|r| r.id() == obs_id).ok_or_else(|| {
            format!(
                "Supporting observation '{}' not found for candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        obs_nodes.push(obs);
    }
```
with:
```rust
    let mut obs_nodes = Vec::new();
    for link in supporting {
        if link.target_domain != "agent_memory" {
            return Err(format!(
                "Candidate '{}' supporting evidence target domain '{}' is invalid (must be 'agent_memory')",
                candidate_id, link.target_domain
            ));
        }
        if link.relation != "PROPOSED_BY" {
            return Err(format!(
                "Candidate '{}' supporting evidence relation '{}' is invalid (must be 'PROPOSED_BY')",
                candidate_id, link.relation
            ));
        }
        let obs_id = link.target_record_id.as_deref().ok_or_else(|| {
            format!(
                "Candidate '{}' supporting evidence link lacks target_record_id",
                candidate_id
            )
        })?;
        let obs = records.iter().find(|r| r.id() == obs_id).ok_or_else(|| {
            format!(
                "Supporting observation '{}' not found for candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        match obs {
            GraphRecord::Node { kind, .. } => {
                if !matches!(kind, NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision) {
                    return Err(format!(
                        "Supporting evidence '{}' has invalid node kind '{:?}' (must be Observation, AgentTurn, or Decision)",
                        obs_id, kind
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "Supporting evidence '{}' is not a node",
                    obs_id
                ));
            }
        }
        obs_nodes.push(obs);
    }
```

**Step 2: Add tests and verify**
Add `test_decide_rejects_non_agent_evidence_in_audit` to `tests/preference_approval.rs`.
Run: `$env:CARGO_INCREMENTAL = "0"; cargo test`
