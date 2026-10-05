//! Issue #169: fold active approved policy into cross-domain context answers.
//!
//! RED phase: these tests exercise the policy-folding surface
//! (`SymbolContext::policy`, scope auto-derivation, glob matching) before the
//! implementation exists.

#![allow(missing_docs, clippy::too_many_lines)]

use aletheia_egregore::{
    DecideRequest, EdgeLabel, EvidenceLink, GraphRecord, NodeKind, UserContextFields,
    UserContextScope, decide_candidate,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, SourceSpan, USER_CONTEXT_SCHEMA_VERSION},
    query::{
        PolicyStatus, TrustClass, TrustIndex, path_glob_matches, policy_scope_applies,
        policy_scope_for_record, symbol_context,
    },
};

const REPO_ID: &str = "codegraph:v1:repo:fixture";
const FILE_POLICY: &str = "codegraph:v1:file:policy-mod";
const SYM_GOVERNED: &str = "codegraph:v1:sym:governed-fn";
const FILE_OTHER: &str = "codegraph:v1:file:other-lib";
const SYM_OTHER: &str = "codegraph:v1:sym:other-fn";
const FILE_PY: &str = "codegraph:v1:file:py-script";
const SYM_PY: &str = "codegraph:v1:sym:py-fn";
const FILE_LONELY: &str = "codegraph:v1:file:lonely-x";
const SYM_LONELY: &str = "codegraph:v1:sym:lonely-fn";

const T1: &str = "2026-09-20T10:00:00Z";
const T2: &str = "2026-09-21T10:00:00Z";
const T4: &str = "2026-09-22T10:00:00Z";
const T5: &str = "2026-09-23T10:00:00Z";
const T6: &str = "2026-09-24T10:00:00Z";
const T7: &str = "2026-09-25T10:00:00Z";
const T8: &str = "2026-09-26T10:00:00Z";
const T9: &str = "2026-09-27T10:00:00Z";

const fn span() -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 40,
        start_line: 1,
        end_line: 5,
        start_column: None,
        end_column: None,
    }
}

fn observation(id: &str, session: &str) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        Some("witness".to_owned()),
        "supporting observation".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        session_id,
        agent_id,
        agent_kind,
        observed_at,
        ingested_at,
        confidence,
        text,
        ..
    } = &mut rec
    {
        // Stamp the agent-memory schema version + domain so the record is
        // loadable by the CLI graph reader (mirrors preference_approval.rs).
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *session_id = Some(session.to_owned());
        *agent_id = Some("agent_1".to_owned());
        *agent_kind = Some("claude-code".to_owned());
        *observed_at = Some("2026-09-01T12:00:00Z".to_owned());
        *ingested_at = Some("2026-09-01T12:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *text = Some("the fmt rule keeps diffs clean".to_owned());
    }
    rec
}

fn support_link(target: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: "agent_memory".to_owned(),
        relation: "PROPOSED_BY".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn candidate(
    id: &str,
    proposed_rule_kind: &str,
    proposed_rule_text: &str,
    scope: UserContextScope,
    build: impl FnOnce(&mut UserContextFields),
) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::PromoteCandidate,
        None,
        None,
        None,
        format!("candidate {id}"),
    );
    if let GraphRecord::Node {
        schema_version,
        confidence,
        evidence_quality,
        user_context,
        ..
    } = &mut rec
    {
        // Stamp the user-context schema version so the record is loadable by
        // the CLI graph reader (mirrors preference_approval.rs).
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *confidence = Some("0.9".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        user_context.proposed_rule_kind = Some(proposed_rule_kind.to_owned());
        user_context.proposed_rule_text = Some(proposed_rule_text.to_owned());
        user_context.scope = Some(scope);
        user_context.supporting_evidence = Some(vec![
            support_link("agent_memory:v1:obs-a1"),
            support_link("agent_memory:v1:obs-a2"),
            support_link("agent_memory:v1:obs-b1"),
        ]);
        user_context.contradicting_evidence = Some(vec![]);
        build(user_context);
    }
    rec
}

/// Runs `decide_candidate` and appends the generated records.
/// Returns `(decision_id, Option<materialized_id>)`.
fn decide(
    records: &mut Vec<GraphRecord>,
    candidate_id: &str,
    outcome: &str,
    at: &str,
) -> (String, Option<String>) {
    let req = DecideRequest {
        candidate_id: candidate_id.to_owned(),
        outcome: outcome.to_owned(),
        edited_rule_text: None,
        rationale: Some("reviewed".to_owned()),
        decided_by: "operator".to_owned(),
        prompt_surface: "cli".to_owned(),
        prompted_to: "operator".to_owned(),
        transaction_time: Some(at.to_owned()),
    };
    let generated = decide_candidate(records, &req).expect("decide_candidate should succeed");
    let mut decision_id = String::new();
    let mut materialized_id = None;
    for rec in &generated {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = rec
        {
            decision_id.clone_from(id);
            materialized_id.clone_from(&user_context.materialized_record_id);
        }
    }
    assert!(!decision_id.is_empty(), "decision record must be generated");
    records.extend(generated);
    (decision_id, materialized_id)
}

fn workflow_scope(path_glob: &str) -> UserContextScope {
    UserContextScope {
        repo: Some(REPO_ID.to_owned()),
        path_glob: Some(path_glob.to_owned()),
        language: None,
        lifecycle_phase: None,
    }
}

fn language_scope(language: &str) -> UserContextScope {
    UserContextScope {
        repo: None,
        path_glob: None,
        language: Some(language.to_owned()),
        lifecycle_phase: None,
    }
}

/// Builds the deterministic AC1 fixture:
///
/// - N=2 active in-scope policy records for `governed_fn`:
///   a `WorkflowRule` scoped to the `src/policy/*` path prefix and a
///   `NamingDecision` scoped to `language = rust`;
/// - M=3 non-active candidates (pending / rejected / deferred) with overlapping scope;
/// - K=3 excluded records (1 superseded, 2 out-of-scope);
/// - three symbols (`governed_fn`, `other_fn`, `py_fn`) plus a policy-free
///   `lonely_fn` for the empty-section case.
fn fixture() -> Vec<GraphRecord> {
    let mut records = Vec::new();

    // ── code graph ──────────────────────────────────────────────────────
    records.push(GraphRecord::node(
        REPO_ID.to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("fixture".to_owned()),
        "fixture repository".to_owned(),
    ));
    let file = |id: &str, path: &str, language: &str| {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::File,
            path.to_owned(),
            span(),
            path.to_owned(),
            language,
            format!("file {path}"),
        )
    };
    let symbol = |id: &str, name: &str, path: &str, language: &str| {
        GraphRecord::syntax_symbol(
            id.to_owned(),
            "function",
            path.to_owned(),
            span(),
            name.to_owned(),
            language,
            0,
            format!("fn {name}"),
        )
    };
    records.push(file(FILE_POLICY, "src/policy/mod.rs", "rust"));
    records.push(symbol(
        SYM_GOVERNED,
        "governed_fn",
        "src/policy/mod.rs",
        "rust",
    ));
    records.push(file(FILE_OTHER, "src/other/lib.rs", "rust"));
    records.push(symbol(SYM_OTHER, "other_fn", "src/other/lib.rs", "rust"));
    records.push(file(FILE_PY, "src/policy/script.py", "python"));
    records.push(symbol(SYM_PY, "py_fn", "src/policy/script.py", "python"));
    records.push(file(FILE_LONELY, "src/lonely/x.rs", "go"));
    records.push(symbol(SYM_LONELY, "lonely_fn", "src/lonely/x.rs", "go"));
    for file_id in [FILE_POLICY, FILE_OTHER, FILE_PY, FILE_LONELY] {
        records.push(GraphRecord::edge(
            EdgeLabel::Contains,
            REPO_ID.to_owned(),
            file_id.to_owned(),
            None,
            "repository contains file".to_owned(),
        ));
    }
    for (file_id, sym_id) in [
        (FILE_POLICY, SYM_GOVERNED),
        (FILE_OTHER, SYM_OTHER),
        (FILE_PY, SYM_PY),
        (FILE_LONELY, SYM_LONELY),
    ] {
        records.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.to_owned(),
            sym_id.to_owned(),
            None,
            "file defines symbol".to_owned(),
        ));
    }

    // ── supporting observations (3 unique, 2 sessions) ──────────────────
    records.push(observation("agent_memory:v1:obs-a1", "sess-a"));
    records.push(observation("agent_memory:v1:obs-a2", "sess-a"));
    records.push(observation("agent_memory:v1:obs-b1", "sess-b"));

    // ── R1: active WorkflowRule scoped to the src/policy/* path prefix ───
    records.push(candidate(
        "user_context:v1:candidate:c1",
        "workflow_rule",
        "Always run `cargo fmt` before committing",
        workflow_scope("src/policy/*"),
        |uc| {
            uc.triggers = Some(vec!["pre_commit".to_owned()]);
            uc.action_summary = Some("Run `cargo fmt` on changed files".to_owned());
        },
    ));
    let (d1, r1) = decide(&mut records, "user_context:v1:candidate:c1", "approved", T1);
    let r1 = r1.expect("approved candidate materializes a record");

    // ── R2: active NamingDecision scoped to language = rust ──────────────
    records.push(candidate(
        "user_context:v1:candidate:c2",
        "naming_decision",
        "PolicyRegistry",
        language_scope("rust"),
        |uc| {
            uc.entity_kind = Some("type".to_owned());
            uc.canonical_name = Some("PolicyRegistry".to_owned());
            uc.alternatives_rejected = Some(vec!["Registry".to_owned()]);
        },
    ));
    let (d2, r2) = decide(&mut records, "user_context:v1:candidate:c2", "approved", T2);
    let r2 = r2.expect("approved candidate materializes a record");

    // ── M: pending / rejected / deferred candidates, overlapping scope ───
    records.push(candidate(
        "user_context:v1:candidate:c3",
        "workflow_rule",
        "Always run `cargo clippy` before pushing",
        workflow_scope("src/policy/*"),
        |uc| {
            uc.triggers = Some(vec!["pre_pr".to_owned()]);
            uc.action_summary = Some("Run `cargo clippy` on changed files".to_owned());
        },
    ));
    records.push(candidate(
        "user_context:v1:candidate:c4",
        "workflow_rule",
        "Always run `cargo doc` before releasing",
        workflow_scope("src/policy/*"),
        |uc| {
            uc.triggers = Some(vec!["pre_pr".to_owned()]);
            uc.action_summary = Some("Run `cargo doc` on changed files".to_owned());
        },
    ));
    let (_d4, _none) = decide(&mut records, "user_context:v1:candidate:c4", "rejected", T4);
    records.push(candidate(
        "user_context:v1:candidate:c5",
        "naming_decision",
        "DeferredRegistry",
        language_scope("rust"),
        |uc| {
            uc.entity_kind = Some("type".to_owned());
            uc.canonical_name = Some("DeferredRegistry".to_owned());
            uc.alternatives_rejected = Some(vec![]);
        },
    ));
    let (_d5, _none) = decide(&mut records, "user_context:v1:candidate:c5", "deferred", T5);

    // ── K: one superseded + two out-of-scope durable records ─────────────
    records.push(candidate(
        "user_context:v1:candidate:c6",
        "workflow_rule",
        "Run `cargo fmt` on Fridays only",
        workflow_scope("src/policy/*"),
        |uc| {
            uc.triggers = Some(vec!["pre_commit".to_owned()]);
            uc.action_summary = Some("Run `cargo fmt` on Fridays".to_owned());
        },
    ));
    let (_d6, r6) = decide(&mut records, "user_context:v1:candidate:c6", "approved", T6);
    let r6 = r6.expect("approved candidate materializes a record");
    // Supersede R6 in favor of R1: active_to set, superseded_by R1.
    for rec in &mut records {
        if let GraphRecord::Node {
            id,
            user_context,
            superseded_by,
            ..
        } = rec
            && id == &r6
        {
            user_context.active_to = Some(T9.to_owned());
            *superseded_by = Some(r1.clone());
        }
    }
    records.push(candidate(
        "user_context:v1:candidate:c7",
        "workflow_rule",
        "Document every public item",
        workflow_scope("src/unrelated/*"),
        |uc| {
            uc.triggers = Some(vec!["pre_pr".to_owned()]);
            uc.action_summary = Some("Write docs for public items".to_owned());
        },
    ));
    let (_d7, _r7) = decide(&mut records, "user_context:v1:candidate:c7", "approved", T7);
    records.push(candidate(
        "user_context:v1:candidate:c8",
        "naming_decision",
        "PyRegistry",
        language_scope("python"),
        |uc| {
            uc.entity_kind = Some("type".to_owned());
            uc.canonical_name = Some("PyRegistry".to_owned());
            uc.alternatives_rejected = Some(vec![]);
        },
    ));
    let (_d8, _r8) = decide(&mut records, "user_context:v1:candidate:c8", "approved", T8);

    // Sanity: the two in-scope durable records really are active.
    for (want_id, want_decision, want_from) in [(&r1, &d1, T1), (&r2, &d2, T2)] {
        let rec = records
            .iter()
            .find(|r| r.id() == want_id.as_str())
            .expect("durable record present");
        if let GraphRecord::Node { user_context, .. } = rec {
            assert_eq!(
                user_context.approval_decision_id.as_deref(),
                Some(want_decision.as_str())
            );
            assert_eq!(user_context.active_from.as_deref(), Some(want_from));
            assert!(user_context.active_to.is_none());
        } else {
            panic!("durable record must be a node");
        }
    }

    records
}

fn policy_ids<'a>(ctx: &'a aletheia_egregore::SymbolContext<'a>) -> Vec<&'a str> {
    ctx.policy
        .iter()
        .map(aletheia_egregore::PolicyEntry::record_id)
        .collect()
}

// ── glob matcher ─────────────────────────────────────────────────────────

#[test]
fn path_glob_matcher_cases() {
    // `*` spans one segment only.
    assert!(path_glob_matches("src/policy/*", "src/policy/mod.rs"));
    assert!(!path_glob_matches("src/policy/*", "src/policy/sub/x.rs"));
    assert!(!path_glob_matches("src/policy/*", "src/policy"));
    // `**` crosses segments.
    assert!(path_glob_matches("src/policy/**", "src/policy/sub/x.rs"));
    assert!(path_glob_matches("src/policy/**", "src/policy/mod.rs"));
    // No wildcards: exact match.
    assert!(path_glob_matches("src/policy/mod.rs", "src/policy/mod.rs"));
    assert!(!path_glob_matches(
        "src/policy/mod.rs",
        "src/policy/other.rs"
    ));
    // `?` matches exactly one non-separator character.
    assert!(path_glob_matches("src/polic?/mod.rs", "src/policy/mod.rs"));
    assert!(!path_glob_matches(
        "src/polic?/mod.rs",
        "src/policyx/mod.rs"
    ));
    // Sibling-prefix bleed is impossible.
    assert!(!path_glob_matches("src/policy/*", "src/policy2/mod.rs"));
}

// ── scope auto-derivation (AC2) ──────────────────────────────────────────

#[test]
fn policy_scope_derives_from_symbol_facts_without_caller_filter() {
    let records = fixture();
    let symbol = records
        .iter()
        .find(|r| r.id() == SYM_GOVERNED)
        .expect("symbol present");
    let scope = policy_scope_for_record(&records, symbol);
    assert_eq!(scope.repo.as_deref(), Some(REPO_ID));
    assert_eq!(scope.path_glob.as_deref(), Some("src/policy/mod.rs"));
    assert_eq!(scope.language.as_deref(), Some("rust"));
    assert_eq!(scope.lifecycle_phase, None);
}

#[test]
fn record_scope_applies_semantics() {
    let target = UserContextScope {
        repo: Some(REPO_ID.to_owned()),
        path_glob: Some("src/policy/mod.rs".to_owned()),
        language: Some("rust".to_owned()),
        lifecycle_phase: None,
    };
    // Path-prefix record scope applies to a path under it.
    assert!(policy_scope_applies(
        &workflow_scope("src/policy/*"),
        &target
    ));
    // Language-only record scope applies when the language matches.
    assert!(policy_scope_applies(&language_scope("rust"), &target));
    // Non-matching path and language scopes do not apply.
    assert!(!policy_scope_applies(
        &workflow_scope("src/other/*"),
        &target
    ));
    assert!(!policy_scope_applies(&language_scope("python"), &target));
    // A record scoped to another repo never applies.
    let mut other_repo = workflow_scope("src/policy/*");
    other_repo.repo = Some("codegraph:v1:repo:other".to_owned());
    assert!(!policy_scope_applies(&other_repo, &target));
}

// ── folding (AC1) ────────────────────────────────────────────────────────

#[test]
fn governed_symbol_folds_exactly_the_in_scope_active_policy() {
    let records = fixture();
    let ctx = symbol_context(&records, "governed_fn");
    assert!(!ctx.is_ambiguous());
    assert!(!ctx.is_no_match());

    let ids = policy_ids(&ctx);
    // N=2 active in-scope records, nothing else: precision = recall = 1.0.
    assert_eq!(
        ids.len(),
        2,
        "expected exactly the 2 in-scope active records"
    );
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    let mut expected: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::WorkflowRule | NodeKind::NamingDecision,
                user_context,
                ..
            } if user_context.active_to.is_none()
                && (user_context
                    .rule_text
                    .as_deref()
                    .is_some_and(|t| t.contains("before committing"))
                    || user_context
                        .canonical_name
                        .as_deref()
                        .is_some_and(|n| n == "PolicyRegistry")) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    expected.sort_unstable();
    assert_eq!(sorted, expected);

    // AC3: every row cites its approval-decision handle and activation time.
    for row in &ctx.policy {
        assert!(
            row.approval_decision_id.starts_with("user_context:"),
            "row must cite its approval-decision handle"
        );
        assert!(!row.active_from.is_empty(), "row must carry active_from");
        assert_eq!(row.status, PolicyStatus::Active);
        assert!(!row.body.is_empty(), "row must carry a human-readable body");
    }
    let kinds: Vec<NodeKind> = ctx.policy.iter().map(|p| p.kind).collect();
    assert!(kinds.contains(&NodeKind::WorkflowRule));
    assert!(kinds.contains(&NodeKind::NamingDecision));
}

#[test]
fn pending_rejected_and_deferred_candidates_never_surface() {
    let records = fixture();
    let ctx = symbol_context(&records, "governed_fn");
    for row in &ctx.policy {
        assert!(
            !matches!(
                row.kind,
                NodeKind::PromoteCandidate | NodeKind::PromotionDecision
            ),
            "policy rows must be durable kinds only"
        );
    }
    let ids = policy_ids(&ctx);
    for excluded in [
        "user_context:v1:candidate:c3",
        "user_context:v1:candidate:c4",
        "user_context:v1:candidate:c5",
    ] {
        assert!(
            !ids.contains(&excluded),
            "candidate {excluded} must not surface in policy"
        );
    }
    // Deferred and rejected decisions exist but authorize nothing.
    let rejected = records.iter().filter(|r| {
        matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::PromotionDecision,
                ..
            }
        ) && match r {
            GraphRecord::Node { user_context, .. } => matches!(
                user_context.outcome.as_deref(),
                Some("rejected" | "deferred")
            ),
            _ => false,
        }
    });
    assert_eq!(rejected.count(), 2);
}

#[test]
fn superseded_and_out_of_scope_records_are_excluded() {
    let records = fixture();
    let ctx = symbol_context(&records, "governed_fn");
    let ids = policy_ids(&ctx);
    // The superseded record (active_to set) must not surface even though its
    // path scope matches the symbol.
    for rec in &records {
        if let GraphRecord::Node {
            id, user_context, ..
        } = rec
            && user_context.active_to.is_some()
        {
            assert!(
                !ids.contains(&id.as_str()),
                "superseded/revoked record {id} must be excluded"
            );
        }
    }
    // Out-of-scope records: path src/unrelated/* and language python.
    for rec in &records {
        if let GraphRecord::Node {
            id,
            kind,
            user_context,
            ..
        } = rec
            && matches!(kind, NodeKind::WorkflowRule | NodeKind::NamingDecision)
            && user_context.active_to.is_none()
            && let Some(scope) = &user_context.scope
            && (scope.path_glob.as_deref() == Some("src/unrelated/*")
                || scope.language.as_deref() == Some("python"))
        {
            assert!(
                !ids.contains(&id.as_str()),
                "out-of-scope record {id} must be excluded"
            );
        }
    }
    assert_eq!(ids.len(), 2);
}

// ── per-symbol selectivity (AC2) ─────────────────────────────────────────

#[test]
fn same_store_different_symbols_see_different_policy() {
    let records = fixture();
    // other_fn: rust, src/other/lib.rs → language-scoped naming decision only.
    let ctx = symbol_context(&records, "other_fn");
    assert_eq!(policy_ids(&ctx).len(), 1);
    assert_eq!(ctx.policy[0].kind, NodeKind::NamingDecision);
    assert_eq!(ctx.policy[0].body, "PolicyRegistry");

    // py_fn: python, src/policy/script.py → path-scoped workflow rule +
    // python-scoped naming decision; the rust naming decision is excluded.
    let ctx = symbol_context(&records, "py_fn");
    let ids = policy_ids(&ctx);
    assert_eq!(ids.len(), 2);
    let kinds: Vec<NodeKind> = ctx.policy.iter().map(|p| p.kind).collect();
    assert!(kinds.contains(&NodeKind::WorkflowRule));
    assert!(kinds.contains(&NodeKind::NamingDecision));
    assert!(
        ctx.policy.iter().all(|p| p.body != "PolicyRegistry"),
        "rust-scoped naming decision must not apply to a python symbol"
    );
}

// ── trust class (AC4) ────────────────────────────────────────────────────

#[test]
fn policy_rows_carry_authorization_derived_trust_class() {
    let records = fixture();
    let ctx = symbol_context(&records, "governed_fn");
    assert!(!ctx.policy.is_empty());
    let trust = TrustIndex::build(&records);
    for row in &ctx.policy {
        let class = trust.classify(row.record);
        // #114 closed vocabulary: user-context policy stays `other` rather
        // than borrowing a truth-bearing label — distinguishable from code
        // facts (`source_derived`) and unverified observations
        // (`agent_unverified`). The authorization basis is the cited
        // approval decision, which the audit-trail gate already verified.
        assert_eq!(class, TrustClass::Other);
        assert_ne!(class, TrustClass::SourceDerived);
        assert_ne!(class, TrustClass::AgentUnverified);
    }
}

// ── empty section is explicit (AC5) ──────────────────────────────────────

#[test]
fn no_matching_policy_yields_explicit_empty_section() {
    let records = fixture();
    let ctx = symbol_context(&records, "lonely_fn");
    assert!(!ctx.is_no_match(), "the symbol itself matches");
    assert!(
        ctx.policy.is_empty(),
        "no active policy applies to lonely_fn"
    );
}

#[test]
fn ambiguous_symbol_yields_empty_policy() {
    let mut records = fixture();
    // A second distinct identity sharing the name.
    records.push(GraphRecord::syntax_symbol(
        "codegraph:v1:sym:governed-fn-dup".to_owned(),
        "function",
        "src/other/dup.rs".to_owned(),
        span(),
        "governed_fn".to_owned(),
        "rust",
        0,
        "duplicate fn governed_fn".to_owned(),
    ));
    let ctx = symbol_context(&records, "governed_fn");
    assert!(ctx.is_ambiguous());
    assert!(
        ctx.policy.is_empty(),
        "ambiguous recall must not attribute policy to any identity"
    );
}

#[test]
fn superseded_without_active_to_still_excluded() {
    // Belt-and-braces: a record carrying `superseded_by` but no `active_to`
    // stamp must not surface, even though `active_policy` only checks
    // `active_to`.
    let mut records = fixture();
    // R6 ("Run `cargo fmt` on Fridays only") is superseded with both stamps;
    // clear `active_to` to simulate the malformed state.
    let mut cleared = false;
    for rec in &mut records {
        if let GraphRecord::Node {
            user_context,
            superseded_by,
            ..
        } = rec
            && superseded_by.is_some()
        {
            user_context.active_to = None;
            cleared = true;
        }
    }
    assert!(cleared, "fixture must contain a superseded durable record");
    let ctx = symbol_context(&records, "governed_fn");
    let bodies: Vec<&str> = ctx.policy.iter().map(|p| p.body).collect();
    assert!(
        !bodies.iter().any(|b| b.contains("Fridays only")),
        "malformed superseded record must not surface: {bodies:?}"
    );
    assert_eq!(
        ctx.policy.len(),
        2,
        "only the two genuinely active records remain: {bodies:?}"
    );
}
